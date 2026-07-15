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

type Conns = Arc<Mutex<Vec<Connection>>>;

/// A running uplink: an iroh endpoint tunneling one network's trunk. Dropping
/// it closes the endpoint and detaches the trunk.
pub struct Uplink {
    ticket: String,
    secret: [u8; 32],
    trunk: Arc<TrunkPort>,
    hub: Arc<NetHub>,
    conns: Conns,
    dial_tx: mpsc::UnboundedSender<EndpointAddr>,
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
    /// re-dials after a drop), so a peer that isn't up yet is fine.
    pub fn dial(&self, ticket: &str) -> Result<()> {
        let t = EndpointTicket::from_str(ticket.trim())
            .map_err(|e| anyhow::anyhow!("bad ticket: {e}"))?;
        let _ = self.dial_tx.send(t.endpoint_addr().clone());
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
            .filter(|c| c.close_reason().is_none())
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
fn register(conn: Connection, conns: &Conns, trunk: &Arc<TrunkPort>) {
    let mut g = conns.lock().unwrap();
    g.retain(|c| c.close_reason().is_none());
    g.push(conn.clone());
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
            register(conn, conns, trunk);
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
            .filter(|c| c.close_reason().is_none())
            .cloned()
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
async fn dialer(
    ep: Endpoint,
    mut rx: mpsc::UnboundedReceiver<EndpointAddr>,
    conns: Conns,
    trunk: Arc<TrunkPort>,
) {
    let mut target: Option<EndpointAddr> = None;
    let mut retry = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            t = rx.recv() => match t {
                Some(addr) => target = Some(addr),
                None => return,
            },
            _ = retry.tick() => {}
        }
        let connected = conns
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.close_reason().is_none());
        if let (Some(addr), false) = (&target, connected) {
            if let Ok(conn) = ep.connect(addr.clone(), ALPN).await {
                register(conn, &conns, &trunk);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::socket::tcp;
    use smoltcp::wire::Ipv4Address;

    fn tcp_socket() -> tcp::Socket<'static> {
        tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 4096]),
            tcp::SocketBuffer::new(vec![0u8; 4096]),
        )
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
}
