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

use tokio::sync::{mpsc, oneshot};
use veilid_core::{
    api_startup, Crypto, DHTSchema, KeyPair, RecordKey, RouteId, RoutingContext, Target, VeilidAPI,
    VeilidConfig, VeilidUpdate, CRYPTO_KIND_VLD0,
};
use wasmtime::Result;
use wk_protocol::NodeId;

use crate::netstack::{NetHub, TrunkPort};

/// First byte of every tunnel message: a raw fabric frame, or a hello carrying
/// the sender's current private-route blob (ack'd so both sides hold a route).
const TAG_FRAME: u8 = 0x00;
const TAG_HELLO: u8 = 0x01;
const TAG_HELLO_ACK: u8 = 0x02;

type Peers = Arc<Mutex<Vec<RouteId>>>;

/// A running Veilid uplink: a dedicated Veilid node tunneling one network's
/// trunk. Dropping it shuts the node down and detaches the trunk.
pub struct VeilidUplink {
    ticket: String,
    identity: String,
    trunk: Arc<TrunkPort>,
    hub: Arc<NetHub>,
    peers: Peers,
    dial_tx: mpsc::UnboundedSender<RecordKey>,
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
                .map_err(|e| wasmtime::Error::msg(format!("bad veilid identity: {e}")))?,
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
    /// retrying while unconnected, so a peer that isn't up yet is fine.
    pub fn dial(&self, ticket: &str) -> Result<()> {
        let key = RecordKey::from_str(ticket.trim())
            .map_err(|e| wasmtime::Error::msg(format!("bad ticket: {e}")))?;
        let _ = self.dial_tx.send(key);
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

/// Add a freshly imported peer route (deduplicated).
fn add_peer(peers: &Peers, route: RouteId) {
    let mut g = peers.lock().unwrap();
    if !g.contains(&route) {
        g.push(route);
    }
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
    mut dial_rx: mpsc::UnboundedReceiver<RecordKey>,
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
                                    add_peer(&peers, route.clone());
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
                            _ => {}
                        }
                    }
                    VeilidUpdate::RouteChange(ch) => {
                        peers
                            .lock()
                            .unwrap()
                            .retain(|r| !ch.dead_remote_routes.contains(r));
                        let ours_died =
                            matches!(&local, Some((id, _)) if ch.dead_routes.contains(id));
                        if ours_died {
                            local = publish_route(api, &rc, &owner, &mut record_open).await;
                            if let Some((_, blob)) = &local {
                                // Refresh every live peer with the new route.
                                let routes: Vec<RouteId> =
                                    peers.lock().unwrap().clone();
                                for r in routes {
                                    let mut m = vec![TAG_HELLO_ACK];
                                    m.extend_from_slice(blob);
                                    let _ = rc.app_message(Target::RouteId(r), m).await;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            t = dial_rx.recv() => {
                let Some(key) = t else { return };
                target = Some(key);
            }
            _ = pump.tick() => {
                let frames = trunk.drain_outbound();
                if frames.is_empty() {
                    continue;
                }
                let routes: Vec<RouteId> = peers.lock().unwrap().clone();
                for frame in frames {
                    let mut m = Vec::with_capacity(frame.len() + 1);
                    m.push(TAG_FRAME);
                    m.extend_from_slice(&frame);
                    for r in &routes {
                        let _ = rc.app_message(Target::RouteId(r.clone()), m.clone()).await;
                    }
                }
            }
            _ = retry.tick() => {
                // Establish (or re-establish) the dialed peer once attached.
                let unconnected = peers.lock().unwrap().is_empty();
                if attached && unconnected {
                    if let Some(key) = &target {
                        if let Some(route) = fetch_peer(api, &rc, key).await {
                            add_peer(&peers, route.clone());
                            if let Some((_, blob)) = &local {
                                let mut m = vec![TAG_HELLO];
                                m.extend_from_slice(blob);
                                let _ = rc.app_message(Target::RouteId(route), m).await;
                            }
                        }
                    }
                }
            }
        }
    }
}
