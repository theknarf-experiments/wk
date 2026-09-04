//! The Iroh uplink: extends a virtual network to a remote fabric over a p2p
//! QUIC connection.
//!
//! An uplink attaches a [`TrunkPort`] to its network and pumps it over an
//! [`iroh`] endpoint: fabric frames ride QUIC **unreliable datagrams** (the
//! WireGuard-over-QUIC shape — smoltcp's TCP does its own loss recovery, so
//! the tunnel must not add head-of-line blocking). Each side shows a *ticket*
//! (its dialable address, hole-punched or relayed by iroh); paste the remote
//! ticket into one side and the two networks behave as one — a node on either
//! fabric reaches nodes on the other at their fabric addresses, transparently
//! to the guests.

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::sync::{mpsc, oneshot};
use wk_protocol::NodeId;

use crate::netstack::{NetHub, TrunkPort};

/// The ALPN for wk fabric tunnels — any wk v1 uplink accepts it.
pub const ALPN: &[u8] = b"wk/fabric/0";

/// A live tunnel connection, tagged with which side opened it.
///
/// The tag exists for *undialing*. Clearing this side's peer means "stop
/// connecting to that remote", so it closes the connections this side dialed —
/// but not one a remote dialed *in*. The remote holds our ticket and never
/// asked us to stop; closing it would only make its dialer re-open the same
/// connection two seconds later. When both sides hold each other's ticket,
/// clearing one of them therefore leaves the tunnel up, carried by the ticket
/// that is still set. Clear both (or delete the uplink) to part company.
struct Conn {
    conn: Connection,
    dialed: bool,
}

type Conns = Arc<Mutex<Vec<Conn>>>;

/// A running uplink: an iroh endpoint tunneling one network's trunk. Dropping
/// it closes the endpoint and detaches the trunk.
pub struct Uplink {
    ticket: String,
    secret: [u8; 32],
    trunk: Arc<TrunkPort>,
    hub: Arc<NetHub>,
    conns: Conns,
    /// `Some(addr)` sets the dial target; `None` clears it (undial).
    dial_tx: mpsc::UnboundedSender<Option<EndpointAddr>>,
    stop: Option<oneshot::Sender<()>>,
}

impl Uplink {
    /// Bind an endpoint and start tunneling network `net`'s trunk. `secret`
    /// (an ed25519 key) keeps the ticket stable across restarts; `relays`
    /// enables n0's public relay/discovery infrastructure (off = direct
    /// addresses only, as in tests). Binding is synchronous — the returned
    /// uplink already knows its ticket.
    pub fn start(
        hub: Arc<NetHub>,
        net: NodeId,
        secret: Option<[u8; 32]>,
        relays: bool,
    ) -> Result<Uplink> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        // Resolve (or mint) the identity here so the caller can read it back
        // via [`Self::secret`] and persist it.
        let secret = secret.unwrap_or_else(|| SecretKey::generate().to_bytes());
        let endpoint = rt.block_on(async {
            let builder = if relays {
                Endpoint::builder(presets::N0)
            } else {
                Endpoint::builder(presets::Minimal)
            };
            builder
                .secret_key(SecretKey::from_bytes(&secret))
                .alpns(vec![ALPN.to_vec()])
                .bind()
                .await
        })?;
        let ticket = EndpointTicket::from(endpoint.addr()).to_string();

        let trunk = hub.attach_trunk(net);
        let conns: Conns = Arc::new(Mutex::new(Vec::new()));
        let (dial_tx, dial_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = oneshot::channel();

        let (t, c, ep) = (trunk.clone(), conns.clone(), endpoint.clone());
        std::thread::Builder::new()
            .name("wk-uplink".into())
            .spawn(move || {
                rt.block_on(async move {
                    tokio::spawn(pump(t.clone(), c.clone()));
                    tokio::spawn(dialer(ep.clone(), dial_rx, c.clone(), t.clone()));
                    tokio::select! {
                        _ = accept_loop(&ep, &c, &t) => {}
                        _ = stop_rx => {}
                    }
                    ep.close().await;
                });
                // Runtime drops here, aborting the pump/dial/read tasks.
            })
            .expect("spawn uplink thread");

        Ok(Uplink {
            ticket,
            secret,
            trunk,
            hub,
            conns,
            dial_tx,
            stop: Some(stop_tx),
        })
    }

    /// This endpoint's dialable address, to paste into the remote side.
    pub fn ticket(&self) -> &str {
        &self.ticket
    }

    /// The ed25519 secret to persist so the ticket survives restarts.
    pub fn secret(&self) -> [u8; 32] {
        self.secret
    }

    /// Dial a remote uplink by its ticket. The dialer keeps retrying (and
    /// re-dials after a drop), so a peer that isn't up yet is fine. An empty
    /// ticket *undials*: it stops re-connecting AND closes the connection it
    /// dialed, so both sides' peer counts fall (a connection a remote dialed in
    /// is left alone — see [`Conn`]).
    pub fn dial(&self, ticket: &str) -> Result<()> {
        let ticket = ticket.trim();
        if ticket.is_empty() {
            let _ = self.dial_tx.send(None);
            return Ok(());
        }
        let t = EndpointTicket::from_str(ticket).map_err(|e| anyhow::anyhow!("bad ticket: {e}"))?;
        let _ = self.dial_tx.send(Some(t.endpoint_addr().clone()));
        Ok(())
    }

    /// Move the uplink to another network (the trunk follows the wire).
    pub fn set_net(&self, net: NodeId) {
        self.trunk.set_net(net);
    }

    /// How many live peer connections the tunnel has.
    pub fn peers(&self) -> usize {
        self.conns
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.conn.close_reason().is_none())
            .count()
    }
}

impl Drop for Uplink {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.hub.detach_trunk(&self.trunk);
    }
}

/// Register a live connection: track it and read its datagrams into the net.
/// `dialed` records that *this* side opened it (see [`Conn`]).
fn register(conn: Connection, conns: &Conns, trunk: &Arc<TrunkPort>, dialed: bool) {
    let mut g = conns.lock().unwrap();
    g.retain(|c| c.conn.close_reason().is_none());
    g.push(Conn {
        conn: conn.clone(),
        dialed,
    });
    let trunk = trunk.clone();
    tokio::spawn(async move {
        while let Ok(frame) = conn.read_datagram().await {
            trunk.inject(frame.to_vec());
        }
    });
}

/// Accept incoming tunnel connections for as long as the endpoint lives.
async fn accept_loop(ep: &Endpoint, conns: &Conns, trunk: &Arc<TrunkPort>) {
    while let Some(incoming) = ep.accept().await {
        if let Ok(conn) = incoming.await {
            register(conn, conns, trunk, false);
        }
    }
}

/// Drain the trunk into every live connection, ~1ms cadence (matching the hub
/// step). A frame larger than the connection's datagram budget is dropped —
/// the fabric MTU (1280, see `VirtualNic::capabilities`) keeps that rare.
async fn pump(trunk: Arc<TrunkPort>, conns: Conns) {
    let mut tick = tokio::time::interval(Duration::from_millis(1));
    loop {
        tick.tick().await;
        let frames = trunk.drain_outbound();
        if frames.is_empty() {
            continue;
        }
        let live: Vec<Connection> = conns
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.conn.close_reason().is_none())
            .map(|c| c.conn.clone())
            .collect();
        for frame in frames {
            for c in &live {
                if c.max_datagram_size().is_some_and(|m| frame.len() <= m) {
                    let _ = c.send_datagram(bytes::Bytes::copy_from_slice(&frame));
                }
            }
        }
    }
}

/// Hold the current dial target and keep a connection to it alive: dial when
/// there's no live connection, re-dial (2s cadence) after drops or failures.
/// Clearing the target also closes what this side dialed, so an undial
/// actually parts the fabrics rather than just declining to re-dial (see
/// [`Conn`] for why an inbound connection survives it).
async fn dialer(
    ep: Endpoint,
    mut rx: mpsc::UnboundedReceiver<Option<EndpointAddr>>,
    conns: Conns,
    trunk: Arc<TrunkPort>,
) {
    let mut target: Option<EndpointAddr> = None;
    let mut retry = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            // `Some(new)` sets/clears the target (undial); `None` = channel closed.
            t = rx.recv() => match t {
                Some(new) => {
                    target = new;
                    if target.is_none() {
                        // Undial: hang up on the peer we dialed. QUIC's
                        // CONNECTION_CLOSE carries this to the remote, so its
                        // peer count drops with ours instead of holding a
                        // connection nobody is using.
                        let mut g = conns.lock().unwrap();
                        for c in g.iter().filter(|c| c.dialed) {
                            c.conn.close(0u32.into(), b"undialed");
                        }
                        g.retain(|c| !c.dialed);
                    }
                }
                None => return,
            },
            _ = retry.tick() => {}
        }
        let connected = conns
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.conn.close_reason().is_none());
        if let (Some(addr), false) = (&target, connected) {
            if let Ok(conn) = ep.connect(addr.clone(), ALPN).await {
                register(conn, &conns, &trunk, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::socket::{tcp, udp};
    use smoltcp::wire::Ipv4Address;

    fn tcp_socket() -> tcp::Socket<'static> {
        tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 4096]),
            tcp::SocketBuffer::new(vec![0u8; 4096]),
        )
    }

    fn udp_socket() -> udp::Socket<'static> {
        let buf = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 4096]);
        udp::Socket::new(buf(), buf())
    }

    /// Two independent fabrics joined by real iroh uplinks over loopback (no
    /// relays): a TCP client on fabric A reaches a server on fabric B through
    /// the QUIC datagram tunnel.
    #[test]
    fn iroh_uplinks_tunnel_tcp_between_fabrics() {
        let hub_a = NetHub::new();
        let hub_b = NetHub::new();
        let net = NodeId::nil();
        let client = hub_a.attach(net, Ipv4Address::new(10, 0, 0, 1), "client");
        let server = hub_b.attach(net, Ipv4Address::new(10, 0, 0, 2), "server");

        let up_a = Uplink::start(hub_a.clone(), net, None, false).unwrap();
        let up_b = Uplink::start(hub_b.clone(), net, None, false).unwrap();
        up_a.dial(up_b.ticket()).unwrap();

        let server_h = {
            let mut g = server.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            g.sockets.get_mut::<tcp::Socket>(h).listen(80).unwrap();
            h
        };
        let client_h = {
            let mut g = client.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            let crate::netstack::NodeStack { iface, sockets, .. } = &mut *g;
            sockets
                .get_mut::<tcp::Socket>(h)
                .connect(iface.context(), (Ipv4Address::new(10, 0, 0, 2), 80), 49152)
                .unwrap();
            h
        };

        let mut sent = false;
        let mut got: Vec<u8> = Vec::new();
        // Generous budget: the QUIC handshake + dial retry can take a moment.
        for _ in 0..5000 {
            {
                let mut g = client.lock().unwrap();
                let cs = g.sockets.get_mut::<tcp::Socket>(client_h);
                if cs.can_send() && !sent {
                    cs.send_slice(b"over quic").unwrap();
                    sent = true;
                }
            }
            {
                let mut g = server.lock().unwrap();
                let ss = g.sockets.get_mut::<tcp::Socket>(server_h);
                if ss.can_recv() {
                    let mut buf = [0u8; 64];
                    let n = ss.recv_slice(&mut buf).unwrap();
                    got.extend_from_slice(&buf[..n]);
                }
            }
            if got.len() >= 9 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(&got, b"over quic");
        assert_eq!(up_a.peers(), 1);
        assert_eq!(up_b.peers(), 1);
    }

    /// A multicast group crosses an uplink: one datagram sent on fabric A
    /// reaches a member of the same virtual network on fabric B, and a member
    /// of fabric A as well.
    ///
    /// The two directions are separate code paths in `NetHub::step`, and the
    /// second is the one worth pinning. A locally-originated group goes out to
    /// the trunks; a group that ARRIVED on a trunk is flooded to local members
    /// but must NOT be sent back out, or two uplinked fabrics would bounce
    /// every SPDP announcement between them forever. That is `from_trunk`.
    #[test]
    fn iroh_uplinks_carry_multicast_between_fabrics() {
        let group = Ipv4Address::new(239, 255, 0, 1);
        let hub_a = NetHub::new();
        let hub_b = NetHub::new();
        let net = NodeId::nil();
        let sender = hub_a.attach(net, Ipv4Address::new(10, 0, 0, 1), "sender");
        let local = hub_a.attach(net, Ipv4Address::new(10, 0, 0, 2), "local");
        let remote = hub_b.attach(net, Ipv4Address::new(10, 0, 0, 3), "remote");

        let up_a = Uplink::start(hub_a.clone(), net, None, false).unwrap();
        let up_b = Uplink::start(hub_b.clone(), net, None, false).unwrap();
        up_a.dial(up_b.ticket()).unwrap();

        let bind = |stack: &crate::netstack::SharedStack| {
            let mut g = stack.lock().unwrap();
            let h = g.sockets.add(udp_socket());
            g.sockets.get_mut::<udp::Socket>(h).bind(7400).unwrap();
            h
        };
        let sender_h = bind(&sender);
        let local_h = bind(&local);
        let remote_h = bind(&remote);

        let recv = |stack: &crate::netstack::SharedStack, h| {
            let mut g = stack.lock().unwrap();
            g.sockets
                .get_mut::<udp::Socket>(h)
                .recv()
                .ok()
                .map(|(d, _)| d.to_vec())
        };

        // Wait for the QUIC handshake before sending, so that exactly ONE
        // datagram is ever put on the wire. That is what lets the counts below
        // mean something.
        for _ in 0..5000 {
            if up_a.peers() == 1 && up_b.peers() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(up_a.peers(), 1, "uplink A never connected");
        assert_eq!(up_b.peers(), 1, "uplink B never connected");

        {
            let mut g = sender.lock().unwrap();
            let s = g.sockets.get_mut::<udp::Socket>(sender_h);
            assert!(s.can_send());
            s.send_slice(b"spdp", (group, 7400)).unwrap();
        }

        // Count copies rather than stopping at the first, and keep counting
        // after both have arrived: one datagram must produce ONE delivery at
        // each member. Anything more is the bounce this design has to avoid --
        // two uplinked fabrics each flooding what the other sent them, which
        // for a group that nobody owns has no natural stopping point.
        let (mut n_local, mut n_remote) = (0, 0);
        let mut settle = 0;
        for _ in 0..3000 {
            if let Some(d) = recv(&local, local_h) {
                assert_eq!(&d, b"spdp");
                n_local += 1;
            }
            if let Some(d) = recv(&remote, remote_h) {
                assert_eq!(&d, b"spdp");
                n_remote += 1;
            }
            if n_local > 0 && n_remote > 0 {
                settle += 1;
                if settle > 500 {
                    break; // both arrived, then half a second of quiet
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(n_local, 1, "same fabric: one datagram, one delivery");
        assert_eq!(n_remote, 1, "across the uplink: one datagram, one delivery");
    }

    /// Wait up to five seconds for both uplinks to agree on a peer count.
    fn settle_at(up_a: &Uplink, up_b: &Uplink, n: usize) {
        for _ in 0..5000 {
            if up_a.peers() == n && up_b.peers() == n {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Clearing the peer hangs up, rather than merely declining to re-dial.
    ///
    /// The distinction is invisible until you look: `dial("")` used to stop the
    /// dialer and leave the QUIC connection open, so `wk ps` went on reporting
    /// `1 peer(s)` on BOTH sides and the two fabrics stayed joined — a node on
    /// A could still reach B after the peer that put them together was gone.
    #[test]
    fn clearing_the_peer_hangs_up_on_both_sides() {
        let hub_a = NetHub::new();
        let hub_b = NetHub::new();
        let net = NodeId::nil();
        let up_a = Uplink::start(hub_a, net, None, false).unwrap();
        let up_b = Uplink::start(hub_b, net, None, false).unwrap();

        up_a.dial(up_b.ticket()).unwrap();
        settle_at(&up_a, &up_b, 1);
        assert_eq!(up_a.peers(), 1, "dialer never connected");
        assert_eq!(up_b.peers(), 1, "acceptor never saw the connection");

        up_a.dial("").unwrap();
        settle_at(&up_a, &up_b, 0);
        assert_eq!(up_a.peers(), 0, "the dialed connection outlived the peer");
        // The remote learns from QUIC's CONNECTION_CLOSE — nothing in wk tells
        // it, which is what makes this work for a peer on another machine.
        assert_eq!(up_b.peers(), 0, "the remote was never told to hang up");

        // And the undial is not permanent: the same ticket dials again.
        up_a.dial(up_b.ticket()).unwrap();
        settle_at(&up_a, &up_b, 1);
        assert_eq!(up_a.peers(), 1, "re-dialing after an undial failed");
    }

    /// An uplink that a remote dialed *in* keeps its connection when its own
    /// (empty) peer is cleared: it has nothing to hang up on, and the remote
    /// still holds its ticket. Clearing a peer must not disconnect a peering
    /// this side never asked for and cannot stop the other end from re-making.
    #[test]
    fn clearing_an_empty_peer_leaves_an_inbound_connection_alone() {
        let hub_a = NetHub::new();
        let hub_b = NetHub::new();
        let net = NodeId::nil();
        let up_a = Uplink::start(hub_a, net, None, false).unwrap();
        let up_b = Uplink::start(hub_b, net, None, false).unwrap();

        up_a.dial(up_b.ticket()).unwrap();
        settle_at(&up_a, &up_b, 1);
        assert_eq!(up_b.peers(), 1, "acceptor never saw the connection");

        // B never dialed anyone, so clearing B's peer is a no-op...
        up_b.dial("").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(up_b.peers(), 1, "B hung up on a connection it did not dial");
        assert_eq!(up_a.peers(), 1, "A's dialed connection was closed by B");
    }
}
