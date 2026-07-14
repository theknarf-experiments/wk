//! wk's userspace network fabric.
//!
//! wk owns the network the way it owns the filesystem (the vfs). Each networked
//! node gets a virtual NIC + its own smoltcp stack; its `wasi:sockets` activity
//! (see [`crate::sockets`]) terminates there and emits real IP packets. A single
//! background hub thread drives every node's stack and routes packets between
//! nodes **on the same virtual network** — so wired nodes reach each other
//! (Docker-bridge style) and unwired nodes (alone on their own network) see
//! nothing. Because we move *packets*, traffic can later be rerouted through
//! middlebox nodes (a VPN/proxy) transparently to the guest.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::time::Duration;
use wk_protocol::NodeId;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp::{Socket as TcpSocket, State as TcpState};
use smoltcp::socket::udp::Socket as UdpSocket;
use smoltcp::time::Instant;
use smoltcp::wire::{
    HardwareAddress, IpAddress, IpCidr, Ipv4Address, Ipv4Packet, Ipv6Address, Ipv6Packet,
};

/// One raw IP packet on the fabric (Medium::Ip — no Ethernet header).
pub type Frame = Vec<u8>;

type Queue = Arc<Mutex<VecDeque<Frame>>>;

fn queue() -> Queue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// A node's virtual network interface: a smoltcp device whose transmitted
/// packets queue in `tx` (drained + routed by the hub) and whose received
/// packets come from `rx` (filled by the hub).
pub struct VirtualNic {
    rx: Queue,
    tx: Queue,
}

impl VirtualNic {
    fn new() -> Self {
        VirtualNic {
            rx: queue(),
            tx: queue(),
        }
    }
    /// Take everything this NIC has transmitted (for the hub to route).
    fn drain_tx(&self) -> Vec<Frame> {
        self.tx.lock().unwrap().drain(..).collect()
    }
    fn deliver(&self, frame: Frame) {
        self.rx.lock().unwrap().push_back(frame);
    }
}

impl Device for VirtualNic {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = 65535;
        caps.checksum = ChecksumCapabilities::ignored();
        caps
    }

    fn receive(&mut self, _t: Instant) -> Option<(RxToken, TxToken)> {
        let frame = self.rx.lock().unwrap().pop_front()?;
        Some((
            RxToken { frame },
            TxToken {
                tx: self.tx.clone(),
            },
        ))
    }

    fn transmit(&mut self, _t: Instant) -> Option<TxToken> {
        Some(TxToken {
            tx: self.tx.clone(),
        })
    }
}

pub struct RxToken {
    frame: Frame,
}
impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.frame)
    }
}

pub struct TxToken {
    tx: Queue,
}
impl phy::TxToken for TxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.tx.lock().unwrap().push_back(buf);
        r
    }
}

/// One node's network stack: its interface, sockets, and NIC, plus the virtual
/// network it's on and its address. Shared between the guest's thread (which
/// does socket operations via [`crate::sockets`]) and the hub thread (which
/// polls it and routes its packets).
pub struct NodeStack {
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    pub device: VirtualNic,
    /// Virtual network id — nodes sharing it can reach each other.
    pub net: NodeId,
    pub ip: Ipv4Address,
    /// The node's fabric IPv6 address (ULA `fd00::/64`), assigned alongside its
    /// IPv4 so guests can use AF_INET6 sockets on the same fabric.
    pub ip6: Ipv6Address,
    /// The node's name, so peers on the same network can resolve it by name.
    pub name: String,
    /// Whether this node may reach the real host network (set when wired to a
    /// Gateway node). Off-fabric connections are bridged to host sockets.
    pub host_access: bool,
    /// Sockets still owned by a live wasi resource, each mapped to the generation
    /// it was created with. When the owner drops, the handle leaves this map (so
    /// derived streams/pollables that outlive it see it as closed instead of
    /// touching a freed handle) and moves to `closing` to be reaped once drained.
    ///
    /// The generation matters because smoltcp reuses freed slot indices: after a
    /// handle is reaped, a new socket can be added under the *same* `SocketHandle`
    /// value. A stale stream captured `(handle, gen)` at creation; checking the
    /// generation as well as membership prevents it from operating on the
    /// unrelated socket that later took the slot.
    live: HashMap<SocketHandle, u64>,
    /// Monotonic generation counter, bumped per tracked socket.
    next_gen: u64,
    /// Sockets whose owner has dropped, awaiting a graceful flush before removal
    /// (TX data + FIN sent). Each carries a tick budget so a stuck socket is
    /// still eventually reaped.
    closing: Vec<(SocketHandle, SockKind, u32)>,
    /// Wakers parked on this stack's pollables; woken each hub tick so guest
    /// socket pollables re-check readiness.
    wakers: Vec<Waker>,
}

/// Which smoltcp socket flavour a handle is, so the hub knows how to tell when
/// it has finished draining before reaping it.
#[derive(Clone, Copy)]
pub enum SockKind {
    Tcp,
    Udp,
}

/// Ticks (~1ms each) to let a closing socket flush before forcing removal.
const CLOSE_TICKS: u32 = 5000;

impl NodeStack {
    /// Park a waker to be woken on the next hub tick (state may have changed).
    pub fn park(&mut self, w: Waker) {
        self.wakers.push(w);
    }

    /// Record a freshly added socket handle as live (owned by a wasi resource),
    /// returning the generation to stamp on the owner and any derived streams.
    pub fn track(&mut self, h: SocketHandle) -> u64 {
        let gen = self.next_gen;
        self.next_gen += 1;
        self.live.insert(h, gen);
        gen
    }

    /// Is `(h, gen)` still the live socket a resource/stream was created against?
    /// False once the owner dropped it or the slot was recycled for a new socket
    /// (which would carry a different generation).
    pub fn is_current(&self, h: SocketHandle, gen: u64) -> bool {
        self.live.get(&h) == Some(&gen)
    }

    /// The owning resource dropped: stop treating the handle as live and queue it
    /// for reaping once it has drained (the caller closes a TCP socket first).
    pub fn begin_close(&mut self, h: SocketHandle, kind: SockKind) {
        if self.live.remove(&h).is_some() {
            self.closing.push((h, kind, CLOSE_TICKS));
        }
    }

    /// Reap closing sockets that have finished draining (TCP fully `Closed`, UDP
    /// send queue empty) or run out their tick budget. Called by the hub.
    fn reap_closing(&mut self) {
        let sockets = &mut self.sockets;
        self.closing.retain_mut(|(h, kind, ticks)| {
            *ticks = ticks.saturating_sub(1);
            let drained = match kind {
                SockKind::Tcp => sockets.get::<TcpSocket>(*h).state() == TcpState::Closed,
                SockKind::Udp => sockets.get::<UdpSocket>(*h).send_queue() == 0,
            };
            if drained || *ticks == 0 {
                sockets.remove(*h);
                false
            } else {
                true
            }
        });
    }
}

pub type SharedStack = Arc<Mutex<NodeStack>>;

/// The network hub: owns every node stack and drives them on a background
/// thread, routing packets between same-network nodes.
pub struct NetHub {
    stacks: Mutex<Vec<SharedStack>>,
    stop: Arc<AtomicBool>,
}

impl NetHub {
    /// Create the hub and start its driver thread.
    pub fn new() -> Arc<NetHub> {
        let hub = Arc::new(NetHub {
            stacks: Mutex::new(Vec::new()),
            stop: Arc::new(AtomicBool::new(false)),
        });
        let driver = hub.clone();
        std::thread::Builder::new()
            .name("wk-net-hub".into())
            .spawn(move || driver.run())
            .expect("spawn net hub");
        hub
    }

    /// Resolve a node `name` to its IPv4 address on virtual network `net`
    /// (fabric DNS) — the first other node with that name on the same network.
    pub fn resolve(&self, net: NodeId, name: &str) -> Option<Ipv4Address> {
        self.stacks.lock().unwrap().iter().find_map(|s| {
            let g = s.lock().unwrap();
            (g.net == net && g.name == name).then_some(g.ip)
        })
    }

    /// Like [`resolve`](Self::resolve) but returns the node's fabric IPv6 address.
    pub fn resolve6(&self, net: NodeId, name: &str) -> Option<Ipv6Address> {
        self.stacks.lock().unwrap().iter().find_map(|s| {
            let g = s.lock().unwrap();
            (g.net == net && g.name == name).then_some(g.ip6)
        })
    }

    /// The fabric IPv6 address for a node, derived from its IPv4 host octet so
    /// the two stay in lock-step (`10.0.0.x` ↔ `fd00::x`).
    fn ula(ip: Ipv4Address) -> Ipv6Address {
        Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, ip.octets()[3] as u16)
    }

    /// Pick a fabric IPv4 address whose host octet isn't taken by any attached
    /// stack, starting from `seed` (so id-derived addresses stay stable when
    /// free). Host octets live in `2..=251`; with all 250 taken the seed is
    /// returned as-is.
    pub fn alloc_ip(&self, seed: u8) -> Ipv4Address {
        let used: std::collections::HashSet<u8> = self
            .stacks
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.lock().unwrap().ip.octets()[3])
            .collect();
        let mut octet = seed.clamp(2, 251);
        for _ in 0..250 {
            if !used.contains(&octet) {
                break;
            }
            octet = 2 + (octet - 1) % 250;
        }
        Ipv4Address::new(10, 0, 0, octet)
    }

    /// Attach a node named `name` to virtual network `net` at address `ip`,
    /// returning its stack (to drive via wasi:sockets).
    pub fn attach(&self, net: NodeId, ip: Ipv4Address, name: &str) -> SharedStack {
        let ip6 = Self::ula(ip);
        let mut device = VirtualNic::new();
        let config = Config::new(HardwareAddress::Ip);
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(ip.into(), 24));
            let _ = addrs.push(IpCidr::new(ip6.into(), 64));
        });
        let stack = Arc::new(Mutex::new(NodeStack {
            iface,
            sockets: SocketSet::new(Vec::new()),
            device,
            net,
            ip,
            ip6,
            name: name.to_string(),
            host_access: false,
            live: HashMap::new(),
            next_gen: 0,
            closing: Vec::new(),
            wakers: Vec::new(),
        }));
        self.stacks.lock().unwrap().push(stack.clone());
        stack
    }

    /// Remove a node's stack from the hub (on node close), so the driver stops
    /// polling it.
    pub fn detach(&self, stack: &SharedStack) {
        self.stacks
            .lock()
            .unwrap()
            .retain(|s| !Arc::ptr_eq(s, stack));
    }

    /// One driver step: poll every stack, route packets between same-network
    /// peers, poll again to deliver, and wake parked pollables. Exposed for
    /// tests; the hub thread calls it in a loop.
    pub fn step(&self) {
        let stacks: Vec<SharedStack> = self.stacks.lock().unwrap().clone();
        let now = Instant::now();

        // Phase 1: poll each stack and collect what it transmitted, tagged with
        // the sender's network so we only route within a network.
        let mut outbound: Vec<(NodeId, Frame)> = Vec::new();
        // Snapshot (net, v4, v6, stack) for delivery lookup.
        let mut routes: Vec<(NodeId, Ipv4Address, Ipv6Address, SharedStack)> = Vec::new();
        for s in &stacks {
            let mut g = s.lock().unwrap();
            let NodeStack {
                iface,
                sockets,
                device,
                net,
                ip,
                ip6,
                ..
            } = &mut *g;
            iface.poll(now, device, sockets);
            let net = *net;
            let ip = *ip;
            let ip6 = *ip6;
            for frame in device.drain_tx() {
                outbound.push((net, frame));
            }
            routes.push((net, ip, ip6, s.clone()));
        }

        // Phase 2: deliver each frame to the same-network node owning the dest IP.
        // The IP version is in the first nibble (4 or 6); parse the dst either way.
        for (net, frame) in outbound {
            let dst: IpAddress = match frame.first().map(|b| b >> 4) {
                Some(4) => match Ipv4Packet::new_checked(&frame[..]) {
                    Ok(p) => p.dst_addr().into(),
                    Err(_) => continue,
                },
                Some(6) => match Ipv6Packet::new_checked(&frame[..]) {
                    Ok(p) => p.dst_addr().into(),
                    Err(_) => continue,
                },
                _ => continue,
            };
            if let Some((_, _, _, stack)) = routes.iter().find(|(n, v4, v6, _)| {
                *n == net && (dst == IpAddress::Ipv4(*v4) || dst == IpAddress::Ipv6(*v6))
            }) {
                stack.lock().unwrap().device.deliver(frame);
            }
            // Off-network / unknown dest: dropped (the isolation boundary).
        }

        // Phase 3: poll again so delivered frames are processed now, reap any
        // drained closing sockets, and wake pollables so guests re-check.
        for s in &stacks {
            let mut g = s.lock().unwrap();
            let NodeStack {
                iface,
                sockets,
                device,
                ..
            } = &mut *g;
            iface.poll(now, device, sockets);
            g.reap_closing();
            for w in g.wakers.drain(..) {
                w.wake();
            }
        }
    }

    fn run(self: Arc<Self>) {
        while !self.stop.load(Ordering::Relaxed) {
            self.step();
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for NetHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::socket::{tcp, udp};

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

    /// Two nodes on the same network exchange a TCP stream over **IPv6** — the
    /// hub routes the fabric ULA (`fd00::/64`) addresses just like IPv4.
    #[test]
    fn same_network_nodes_talk_tcp_ipv6() {
        let hub = NetHub::new();
        let net = NodeId::nil();
        let client = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "client");
        let server = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "server");
        let server_ip6 = server.lock().unwrap().ip6;

        let server_h = {
            let mut g = server.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            g.sockets.get_mut::<tcp::Socket>(h).listen(80).unwrap();
            h
        };
        let client_h = {
            let mut g = client.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            let NodeStack { iface, sockets, .. } = &mut *g;
            sockets
                .get_mut::<tcp::Socket>(h)
                .connect(iface.context(), (server_ip6, 80), 49152)
                .unwrap();
            h
        };

        let mut sent = false;
        let mut got: Vec<u8> = Vec::new();
        for _ in 0..500 {
            hub.step();

            {
                let mut g = client.lock().unwrap();
                let cs = g.sockets.get_mut::<tcp::Socket>(client_h);
                if cs.can_send() && !sent {
                    cs.send_slice(b"hello v6 net").unwrap();
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
            if got.len() >= 12 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(&got, b"hello v6 net");
    }

    /// Two nodes on the same virtual network exchange a UDP datagram via the hub
    /// — UDP rides the same packet routing as TCP.
    #[test]
    fn same_network_nodes_talk_udp() {
        let server_ip = Ipv4Address::new(10, 0, 0, 2);
        let client_ip = Ipv4Address::new(10, 0, 0, 1);
        let hub = NetHub::new();
        let net = NodeId::nil();
        let client = hub.attach(net, client_ip, "client");
        let server = hub.attach(net, server_ip, "server");

        let server_h = {
            let mut g = server.lock().unwrap();
            let h = g.sockets.add(udp_socket());
            g.sockets.get_mut::<udp::Socket>(h).bind(4242).unwrap();
            h
        };
        let client_h = {
            let mut g = client.lock().unwrap();
            let h = g.sockets.add(udp_socket());
            g.sockets.get_mut::<udp::Socket>(h).bind(49152).unwrap();
            h
        };

        let mut sent = false;
        let mut got: Vec<u8> = Vec::new();
        for _ in 0..500 {
            hub.step();
            {
                let mut g = client.lock().unwrap();
                let cs = g.sockets.get_mut::<udp::Socket>(client_h);
                if cs.can_send() && !sent {
                    cs.send_slice(b"hello udp", (server_ip, 4242)).unwrap();
                    sent = true;
                }
            }
            {
                let mut g = server.lock().unwrap();
                let ss = g.sockets.get_mut::<udp::Socket>(server_h);
                if let Ok((data, meta)) = ss.recv() {
                    got.extend_from_slice(data);
                    assert_eq!(meta.endpoint.port, 49152);
                }
            }
            if got.len() >= 9 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(&got, b"hello udp");
    }

    /// Two nodes on the same virtual network exchange a TCP stream, driven by the
    /// hub's `step` — exercises the NIC, the per-network routing, and the stacks.
    #[test]
    fn same_network_nodes_talk_tcp() {
        let server_ip = Ipv4Address::new(10, 0, 0, 2);
        let hub = NetHub::new();
        let net = NodeId::nil();
        let client = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "client");
        let server = hub.attach(net, server_ip, "server");

        let server_h = {
            let mut g = server.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            g.sockets.get_mut::<tcp::Socket>(h).listen(80).unwrap();
            h
        };
        let client_h = {
            let mut g = client.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            let NodeStack { iface, sockets, .. } = &mut *g;
            sockets
                .get_mut::<tcp::Socket>(h)
                .connect(iface.context(), (server_ip, 80), 49152)
                .unwrap();
            h
        };

        let mut sent = false;
        let mut got: Vec<u8> = Vec::new();
        for _ in 0..500 {
            hub.step();
            {
                let mut g = client.lock().unwrap();
                let cs = g.sockets.get_mut::<tcp::Socket>(client_h);
                if cs.can_send() && !sent {
                    cs.send_slice(b"hello wk net").unwrap();
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
            if got.len() >= 12 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(&got, b"hello wk net");
    }

    /// Nodes on DIFFERENT virtual networks can't reach each other, even at the
    /// same address — the isolation boundary (off-network packets are dropped).
    #[test]
    fn different_networks_are_isolated() {
        let server_ip = Ipv4Address::new(10, 0, 0, 2);
        let hub = NetHub::new();
        let net = NodeId::nil();
        let client = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "client"); // net 1
        let net2 = NodeId::new();
        let server = hub.attach(net2, server_ip, "server"); // net 2 — isolated

        let server_h = {
            let mut g = server.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            g.sockets.get_mut::<tcp::Socket>(h).listen(80).unwrap();
            h
        };
        let client_h = {
            let mut g = client.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            let NodeStack { iface, sockets, .. } = &mut *g;
            sockets
                .get_mut::<tcp::Socket>(h)
                .connect(iface.context(), (server_ip, 80), 49152)
                .unwrap();
            h
        };

        for _ in 0..200 {
            hub.step();
            std::thread::sleep(Duration::from_millis(1));
        }
        // The connection never establishes and the server never leaves Listen.
        let cstate = client
            .lock()
            .unwrap()
            .sockets
            .get::<tcp::Socket>(client_h)
            .state();
        let sstate = server
            .lock()
            .unwrap()
            .sockets
            .get::<tcp::Socket>(server_h)
            .state();
        assert_ne!(
            cstate,
            tcp::State::Established,
            "client on net 1 must not connect to a server on net 2 (was {cstate:?})"
        );
        assert_eq!(
            sstate,
            tcp::State::Listen,
            "server on net 2 must not see the net-1 client (was {sstate:?})"
        );
    }
}
