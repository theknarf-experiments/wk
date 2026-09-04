//! The Veilid uplink: extends a virtual network to a remote fabric over
//! [Veilid](https://veilid.com)'s onion-routed p2p network — the
//! privacy-preserving sibling of the iroh uplink (see [`crate::uplink`]).
//!
//! Fabric frames ride `app_message`s over Veilid **private routes**, so
//! neither side learns the other's IP. The rendezvous is a DHT record: each
//! uplink owns one (its key is derived from a persisted owner keypair, so the
//! *ticket* — the record key string, `VLD0:…` — is stable across restarts) and
//! publishes its current route blob there. Dialing a ticket reads the blob,
//! imports the route, and sends a hello carrying our own blob so the peer can
//! talk back. Private routes die routinely as the network churns; both sides
//! re-allocate, re-publish, and re-hello.

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, oneshot};
use veilid_core::{
    api_startup, Crypto, DHTSchema, KeyPair, RecordKey, RouteId, RoutingContext, Target, VeilidAPI,
    VeilidConfig, VeilidUpdate, CRYPTO_KIND_VLD0,
};
use wk_protocol::NodeId;

use crate::netstack::{NetHub, TrunkPort};

/// First byte of every tunnel message: a raw fabric frame, or a hello carrying
/// the sender's current private-route blob (ack'd so both sides hold a route).
const TAG_FRAME: u8 = 0x00;
const TAG_HELLO: u8 = 0x01;
const TAG_HELLO_ACK: u8 = 0x02;
/// Goodbye, carrying the sender's route blob so the receiver can identify
/// which of its peer routes just went away. A peer too old to know the tag
/// ignores it (the message match has a catch-all) and prunes the dead route
/// the slow way, when a send to it starts failing.
const TAG_BYE: u8 = 0x03;

/// A peer route, tagged with which side sought it out — the Veilid twin of
/// [`crate::uplink::Conn`], and clearing the peer means the same thing here:
/// let go of the routes we dialed, keep the ones a remote brought us.
#[derive(Clone)]
struct Peer {
    route: RouteId,
    dialed: bool,
}

type Peers = Arc<Mutex<Vec<Peer>>>;

/// A running Veilid uplink: a dedicated Veilid node tunneling one network's
/// trunk. Dropping it shuts the node down and detaches the trunk.
pub struct VeilidUplink {
    ticket: String,
    identity: String,
    trunk: Arc<TrunkPort>,
    hub: Arc<NetHub>,
    peers: Peers,
    /// `Some(key)` sets the dial target; `None` clears it (undial).
    dial_tx: mpsc::UnboundedSender<Option<RecordKey>>,
    stop: Option<oneshot::Sender<()>>,
}

impl VeilidUplink {
    /// Start a Veilid node (namespaced per uplink node, so several can run in
    /// one process) and begin tunneling network `net`'s trunk. `identity` is
    /// the persisted DHT owner keypair string; `None` generates a fresh one —
    /// read it back via [`Self::identity`] to persist. Returns once the node's
    /// stores are open and the ticket is derived; attaching to the Veilid
    /// network (and route publication) continues in the background.
    pub fn start(
        hub: Arc<NetHub>,
        net: NodeId,
        identity: Option<&str>,
        node: NodeId,
    ) -> Result<VeilidUplink> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;

        let owner: KeyPair = match identity {
            Some(s) => KeyPair::from_str(s.trim())
                .map_err(|e| anyhow::anyhow!("bad veilid identity: {e}"))?,
            None => Crypto::generate_keypair(CRYPTO_KIND_VLD0)?,
        };
        let identity = owner.to_string();

        let (utx, updates) = mpsc::unbounded_channel();
        let update_cb: veilid_core::UpdateCallback = Arc::new(move |u| {
            let _ = utx.send(u);
        });

        // A throwaway on-disk store per uplink node. Losing it is fine: the
        // ticket derives from the owner keypair, not from stored state.
        let dir = std::env::temp_dir().join(format!("wk-veilid-{node}"));
        let mut config = VeilidConfig {
            program_name: "wk".into(),
            namespace: node.to_string(),
            ..VeilidConfig::default()
        };
        config.protected_store.directory = dir.join("protected").to_string_lossy().into_owned();
        config.protected_store.always_use_insecure_storage = true;
        config.protected_store.allow_insecure_fallback = true;
        config.table_store.directory = dir.join("table").to_string_lossy().into_owned();
        config.block_store.directory = dir.join("block").to_string_lossy().into_owned();

        let api = rt.block_on(api_startup(update_cb, config))?;
        // The record key is derived locally from the owner key — the ticket is
        // known (and stable) before the network is even attached.
        let ticket = rt
            .block_on(api.get_dht_record_key(DHTSchema::dflt(1)?, owner.key(), None))?
            .to_string();

        let trunk = hub.attach_trunk(net);
        let peers: Peers = Arc::new(Mutex::new(Vec::new()));
        let (dial_tx, dial_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = oneshot::channel();

        let (t, p) = (trunk.clone(), peers.clone());
        std::thread::Builder::new()
            .name("wk-veilid".into())
            .spawn(move || {
                rt.block_on(async move {
                    tokio::select! {
                        _ = drive(&api, owner, updates, t, p, dial_rx) => {}
                        _ = stop_rx => {}
                    }
                    let _ = api.detach().await;
                    api.shutdown().await;
                });
            })
            .expect("spawn veilid thread");

        Ok(VeilidUplink {
            ticket,
            identity,
            trunk,
            hub,
            peers,
            dial_tx,
            stop: Some(stop_tx),
        })
    }

    /// This uplink's DHT record key (`VLD0:…`), to paste into the remote side.
    pub fn ticket(&self) -> &str {
        &self.ticket
    }

    /// The owner keypair string to persist so the ticket survives restarts.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Dial a remote uplink by its ticket (a DHT record key). The driver keeps
    /// retrying while unconnected, so a peer that isn't up yet is fine. An empty
    /// ticket *undials*: it stops re-connecting AND releases the routes it
    /// dialed, telling the far side ([`TAG_BYE`]) so its count falls too. A
    /// route a remote brought us is left alone — see [`Peer`].
    pub fn dial(&self, ticket: &str) -> Result<()> {
        let ticket = ticket.trim();
        if ticket.is_empty() {
            let _ = self.dial_tx.send(None);
            return Ok(());
        }
        let key = RecordKey::from_str(ticket).map_err(|e| anyhow::anyhow!("bad ticket: {e}"))?;
        let _ = self.dial_tx.send(Some(key));
        Ok(())
    }

    /// Move the uplink to another network (the trunk follows the wire).
    pub fn set_net(&self, net: NodeId) {
        self.trunk.set_net(net);
    }

    /// How many live peer routes the tunnel has.
    pub fn peers(&self) -> usize {
        self.peers.lock().unwrap().len()
    }
}

impl Drop for VeilidUplink {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.hub.detach_trunk(&self.trunk);
    }
}

/// Add a freshly imported peer route (deduplicated). `dialed` records that we
/// went looking for this one rather than being introduced to it. Importing the
/// same blob twice yields the same [`RouteId`], which is what makes both the
/// dedup and [`TAG_BYE`]'s "drop the route this blob names" work.
fn add_peer(peers: &Peers, route: RouteId, dialed: bool) {
    let mut g = peers.lock().unwrap();
    if let Some(p) = g.iter_mut().find(|p| p.route == route) {
        // Re-dialing a route we were introduced to makes it ours to hang up on.
        p.dialed |= dialed;
        return;
    }
    g.push(Peer { route, dialed });
}

/// Allocate a private route and publish its blob in our DHT record, returning
/// the route id + blob. Creates (or re-opens) the record on first use.
async fn publish_route(
    api: &VeilidAPI,
    rc: &RoutingContext,
    owner: &KeyPair,
    record_open: &mut bool,
) -> Option<(RouteId, Vec<u8>)> {
    let rb = api.new_private_route().await.ok()?;
    let key = api
        .get_dht_record_key(DHTSchema::dflt(1).ok()?, owner.key(), None)
        .await
        .ok()?;
    if !*record_open {
        // Deterministic with the owner keypair: create yields our stable key,
        // and if the record already exists on this store, open it instead.
        if rc
            .create_dht_record(
                CRYPTO_KIND_VLD0,
                DHTSchema::dflt(1).ok()?,
                Some(owner.clone()),
            )
            .await
            .is_err()
        {
            let _ = rc
                .open_dht_record(key.clone(), Some(owner.clone()))
                .await
                .ok()?;
        }
        *record_open = true;
    }
    rc.set_dht_value(key, 0, rb.blob.clone(), None).await.ok()?;
    Some((rb.route_id, rb.blob))
}

/// Send an app message to a peer route, pruning the route from `peers` if the
/// send fails — a dead or stale route (the far side rotated it and Veilid
/// hasn't told us) drops out lazily, and once `peers` empties the retry tick
/// re-fetches the peer's current blob. Also stops routes accumulating across
/// route rotations.
async fn send_to(rc: &RoutingContext, peers: &Peers, route: &RouteId, msg: Vec<u8>) {
    if rc
        .app_message(Target::RouteId(route.clone()), msg)
        .await
        .is_err()
    {
        peers.lock().unwrap().retain(|p| &p.route != route);
    }
}

/// Read a remote uplink's current route blob from its DHT record and import
/// it, returning the peer route.
async fn fetch_peer(api: &VeilidAPI, rc: &RoutingContext, key: &RecordKey) -> Option<RouteId> {
    // Open is idempotent enough for our use; a second open just errors.
    let _ = rc.open_dht_record(key.clone(), None).await;
    let value = rc.get_dht_value(key.clone(), 0, true).await.ok()??;
    api.import_remote_private_route(value.data().to_vec()).ok()
}

/// The uplink driver: attach, publish our route, then shuttle frames between
/// the trunk and the peers while healing route churn.
async fn drive(
    api: &VeilidAPI,
    owner: KeyPair,
    mut updates: mpsc::UnboundedReceiver<VeilidUpdate>,
    trunk: Arc<TrunkPort>,
    peers: Peers,
    mut dial_rx: mpsc::UnboundedReceiver<Option<RecordKey>>,
) {
    let Ok(rc) = api.routing_context() else {
        return;
    };
    let _ = api.attach().await;

    let mut attached = false;
    let mut record_open = false;
    // Our current private route (dies with network churn; rebuilt on demand).
    let mut local: Option<(RouteId, Vec<u8>)> = None;
    let mut target: Option<RecordKey> = None;

    let mut pump = tokio::time::interval(Duration::from_millis(1));
    let mut retry = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            u = updates.recv() => {
                let Some(u) = u else { return };
                match u {
                    VeilidUpdate::Attachment(a)
                        if !attached && a.state.is_attached() && a.public_internet_ready =>
                    {
                        attached = true;
                        local = publish_route(api, &rc, &owner, &mut record_open).await;
                    }
                    VeilidUpdate::AppMessage(m) => {
                        let msg = m.message();
                        match msg.first() {
                            Some(&TAG_FRAME) => trunk.inject(msg[1..].to_vec()),
                            Some(&(TAG_HELLO | TAG_HELLO_ACK)) => {
                                let ack = msg[0] == TAG_HELLO;
                                if let Ok(route) =
                                    api.import_remote_private_route(msg[1..].to_vec())
                                {
                                    add_peer(&peers, route.clone(), false);
                                    // Answer a hello with our blob so the peer
                                    // holds a live route back to us.
                                    if ack {
                                        if let Some((_, blob)) = &local {
                                            let mut m = vec![TAG_HELLO_ACK];
                                            m.extend_from_slice(blob);
                                            let _ = rc
                                                .app_message(Target::RouteId(route), m)
                                                .await;
                                        }
                                    }
                                }
                            }
                            Some(&TAG_BYE) => {
                                // The peer cleared its dial target. Let go of
                                // the route it is naming — it re-imports to the
                                // same id we stored — so the count here falls
                                // with the count there and we stop pushing
                                // frames down a tunnel nobody is reading.
                                if let Ok(route) =
                                    api.import_remote_private_route(msg[1..].to_vec())
                                {
                                    peers.lock().unwrap().retain(|p| p.route != route);
                                }
                            }
                            _ => {}
                        }
                    }
                    VeilidUpdate::RouteChange(ch) => {
                        peers
                            .lock()
                            .unwrap()
                            .retain(|p| !ch.dead_remote_routes.contains(&p.route));
                        let ours_died =
                            matches!(&local, Some((id, _)) if ch.dead_routes.contains(id));
                        if ours_died {
                            local = publish_route(api, &rc, &owner, &mut record_open).await;
                            if let Some((_, blob)) = &local {
                                // Refresh every live peer with the new route.
                                let routes: Vec<RouteId> =
                                    peers.lock().unwrap().iter().map(|p| p.route.clone()).collect();
                                for r in routes {
                                    let mut m = vec![TAG_HELLO_ACK];
                                    m.extend_from_slice(blob);
                                    send_to(&rc, &peers, &r, m).await;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            t = dial_rx.recv() => {
                // `Some(new)` sets/clears the target (undial); `None` = closed.
                let Some(new) = t else { return };
                target = new;
                if target.is_none() {
                    // Undial: let go of the routes we dialed, and say so. There
                    // is no connection here to close and no equivalent of QUIC's
                    // CONNECTION_CLOSE, so without the goodbye the far side
                    // would keep a route to us and keep sending frames into a
                    // fabric that had disowned it.
                    let dialed: Vec<RouteId> = {
                        let mut g = peers.lock().unwrap();
                        let d = g.iter().filter(|p| p.dialed)
                            .map(|p| p.route.clone()).collect();
                        g.retain(|p| !p.dialed);
                        d
                    };
                    if let Some((_, blob)) = &local {
                        let mut m = vec![TAG_BYE];
                        m.extend_from_slice(blob);
                        for r in &dialed {
                            let _ = rc.app_message(Target::RouteId(r.clone()), m.clone()).await;
                        }
                    }
                }
            }
            _ = pump.tick() => {
                let frames = trunk.drain_outbound();
                if frames.is_empty() {
                    continue;
                }
                let routes: Vec<RouteId> =
                    peers.lock().unwrap().iter().map(|p| p.route.clone()).collect();
                for frame in frames {
                    let mut m = Vec::with_capacity(frame.len() + 1);
                    m.push(TAG_FRAME);
                    m.extend_from_slice(&frame);
                    for r in &routes {
                        // Prune a route that errors — a stale/dead route drops
                        // out and the retry tick re-fetches the peer's blob.
                        send_to(&rc, &peers, r, m.clone()).await;
                    }
                }
            }
            _ = retry.tick() => {
                // Self-heal a route publish that failed transiently: without
                // this, a single failure left `local` None forever (no code
                // path republished) and the DHT record held a dead blob.
                if attached && local.is_none() {
                    local = publish_route(api, &rc, &owner, &mut record_open).await;
                }
                // Establish (or re-establish) the dialed peer once attached —
                // `peers` empties when a route is pruned, so a rotated peer is
                // re-fetched here.
                let unconnected = peers.lock().unwrap().is_empty();
                if attached && unconnected {
                    if let Some(key) = &target {
                        if let Some(route) = fetch_peer(api, &rc, key).await {
                            add_peer(&peers, route.clone(), true);
                            if let Some((_, blob)) = &local {
                                let mut m = vec![TAG_HELLO];
                                m.extend_from_slice(blob);
                                send_to(&rc, &peers, &route, m).await;
                            }
                        }
                    }
                }
            }
        }
    }
}
