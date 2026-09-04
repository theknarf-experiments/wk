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
    HardwareAddress, IpAddress, IpCidr, IpListenEndpoint, Ipv4Address, Ipv4Packet, Ipv6Address,
    Ipv6Packet,
};

/// One raw IP packet on the fabric (Medium::Ip — no Ethernet header).
pub type Frame = Vec<u8>;

/// The destination address of a raw fabric frame. The IP version is in the
/// first nibble (4 or 6); parse the dst either way, `None` for garbage.
fn frame_dst(frame: &[u8]) -> Option<IpAddress> {
    match frame.first().map(|b| b >> 4) {
        Some(4) => Ipv4Packet::new_checked(frame)
            .ok()
            .map(|p| p.dst_addr().into()),
        Some(6) => Ipv6Packet::new_checked(frame)
            .ok()
            .map(|p| p.dst_addr().into()),
        _ => None,
    }
}

/// A multicast group — IPv4 `224.0.0.0/4`, IPv6 `ff00::/8`.
///
/// A group is not any node's address, so the owner lookup in `step` can never
/// match one. Such a frame goes to every member of the network instead; see
/// the multicast branch there.
fn is_multicast(dst: IpAddress) -> bool {
    match dst {
        IpAddress::Ipv4(v4) => v4.octets()[0] & 0xf0 == 0xe0,
        IpAddress::Ipv6(v6) => v6.octets()[0] == 0xff,
    }
}

/// A destination a node reaches by talking to itself: `127.0.0.0/8` or `::1`.
/// Such a frame never leaves the node — the hub loops it straight back into the
/// sender's own receive queue, so `localhost` works inside a node the way it
/// does on a real host (a server and a client in one node can connect).
fn is_loopback(dst: IpAddress) -> bool {
    match dst {
        IpAddress::Ipv4(a) => a.octets()[0] == 127,
        IpAddress::Ipv6(a) => a == Ipv6Address::LOCALHOST,
    }
}

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
        // The IPv6 minimum link MTU. Frames must fit in one QUIC datagram when
        // a network is extended to a remote fabric (see [`crate::uplink`]), and
        // ~1200 bytes is the safe inner payload on a 1500-MTU path — so cap
        // every fabric link rather than let local TCP negotiate 64K segments
        // that would drop the moment they cross an uplink.
        caps.max_transmission_unit = 1280;
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

/// An IP family on the fabric — which of a node's two addresses (`10.0.0.x` /
/// `fd00::x`) a socket lives on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IpFamily {
    V4,
    V6,
}

impl IpFamily {
    /// The family of a concrete address.
    pub fn of(addr: IpAddress) -> IpFamily {
        match addr {
            IpAddress::Ipv4(_) => IpFamily::V4,
            IpAddress::Ipv6(_) => IpFamily::V6,
        }
    }
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

    /// The local endpoints a TCP listener on `port` must cover for a socket of
    /// `family` bound to `bound`.
    ///
    /// Family-scoped listening is what keeps a v6 SYN off a v4-bound listener:
    /// smoltcp matches a listener with a concrete address only against packets
    /// to that address (and RSTs anything unmatched), while a port-only
    /// listener (`listen(port)`) matches EVERY local address — both families —
    /// which would hand a v4-bound guest socket a v6 peer. A guest runtime may
    /// rightly refuse to represent that (wasi-libc's accept() `abort()`s
    /// converting the mismatched family to a sockaddr), and wasi 0.2 sockets
    /// are never dual-stack ("WASI IPv6 sockets are always v6-only"). Refused
    /// at SYN time, a dual-family client (curl's happy eyeballs) simply falls
    /// back to the other address.
    ///
    /// A concrete `bound` address (of the right family) narrows the listener to
    /// that address alone; unspecified (`0.0.0.0` / `::`, or no address) covers
    /// the node's fabric address plus loopback in that family.
    pub fn listen_endpoints(
        &self,
        family: IpFamily,
        bound: Option<IpAddress>,
        port: u16,
    ) -> Vec<IpListenEndpoint> {
        match bound {
            Some(addr) if !addr.is_unspecified() && IpFamily::of(addr) == family => {
                vec![IpListenEndpoint {
                    addr: Some(addr),
                    port,
                }]
            }
            _ => {
                let addrs: [IpAddress; 2] = match family {
                    IpFamily::V4 => [self.ip.into(), Ipv4Address::new(127, 0, 0, 1).into()],
                    IpFamily::V6 => [self.ip6.into(), Ipv6Address::LOCALHOST.into()],
                };
                addrs
                    .into_iter()
                    .map(|addr| IpListenEndpoint {
                        addr: Some(addr),
                        port,
                    })
                    .collect()
            }
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

/// A trunk port on a virtual network: it receives every frame on its net whose
/// destination is no local stack (which would otherwise drop at the isolation
/// boundary), and can inject frames from elsewhere — a remote fabric, a
/// middlebox — into the net. Whoever attaches the trunk (an Iroh uplink, a VPN
/// node) shuttles frames between `drain_outbound` and `inject`.
pub struct TrunkPort {
    /// The network this trunk extends (follows rewiring via [`Self::set_net`]).
    net: Mutex<NodeId>,
    /// Frames leaving the local net (no local owner for the dst) for the
    /// remote side.
    outbound: Queue,
    /// Frames arriving from the remote side, delivered into the net on the
    /// next hub step. Never re-trunked (split horizon), so two joined fabrics
    /// can't loop a frame back and forth.
    inbound: Queue,
}

impl TrunkPort {
    pub fn net(&self) -> NodeId {
        *self.net.lock().unwrap()
    }
    pub fn set_net(&self, net: NodeId) {
        *self.net.lock().unwrap() = net;
    }
    /// Take the frames headed for the remote side.
    pub fn drain_outbound(&self) -> Vec<Frame> {
        self.outbound.lock().unwrap().drain(..).collect()
    }
    /// Hand a frame from the remote side to the local net.
    pub fn inject(&self, frame: Frame) {
        self.inbound.lock().unwrap().push_back(frame);
    }
    fn drain_inbound(&self) -> Vec<Frame> {
        self.inbound.lock().unwrap().drain(..).collect()
    }
    fn deliver_outbound(&self, frame: Frame) {
        self.outbound.lock().unwrap().push_back(frame);
    }
}

/// A router: a node that bridges the virtual networks it is wired to, so a
/// frame with no owner on its own net can still reach one on a neighbour.
///
/// Bridging is this cheap because every fabric address is unique across the
/// whole process (see [`NetHub::alloc_ip`]) — there are no per-net subnets to
/// translate between, so a router grants *permission* to cross rather than
/// rewriting anything. Which is also why isolation is unaffected until someone
/// wires one: a net no router names is reachable from nowhere else.
pub struct RouterPort {
    /// The networks this router joins (follows rewiring via [`Self::set_nets`]).
    nets: Mutex<Vec<NodeId>>,
}

impl RouterPort {
    pub fn nets(&self) -> Vec<NodeId> {
        self.nets.lock().unwrap().clone()
    }
    pub fn set_nets(&self, nets: Vec<NodeId>) {
        *self.nets.lock().unwrap() = nets;
    }
}

/// The network hub: owns every node stack and drives them on a background
/// thread, routing packets between same-network nodes.
pub struct NetHub {
    stacks: Mutex<Vec<SharedStack>>,
    trunks: Mutex<Vec<Arc<TrunkPort>>>,
    routers: Mutex<Vec<Arc<RouterPort>>>,
    stop: Arc<AtomicBool>,
}

impl NetHub {
    /// Create the hub and start its driver thread.
    pub fn new() -> Arc<NetHub> {
        let hub = Arc::new(NetHub {
            stacks: Mutex::new(Vec::new()),
            trunks: Mutex::new(Vec::new()),
            routers: Mutex::new(Vec::new()),
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
        self.named(net, name, |g| g.ip)
    }

    /// Like [`resolve`](Self::resolve) but returns the node's fabric IPv6 address.
    pub fn resolve6(&self, net: NodeId, name: &str) -> Option<Ipv6Address> {
        self.named(net, name, |g| g.ip6)
    }

    /// Fabric DNS: the first node called `name` on `net`, else the first on a
    /// net a router bridges it to. Local names shadow routed ones, so bridging
    /// two networks can never change who an existing name already meant.
    fn named<T>(&self, net: NodeId, name: &str, pick: impl Fn(&NodeStack) -> T) -> Option<T> {
        let stacks = self.stacks.lock().unwrap().clone();
        let on = |want: NodeId| {
            stacks.iter().find_map(|s| {
                let g = s.lock().unwrap();
                (g.net == want && g.name == name).then(|| pick(&g))
            })
        };
        on(net).or_else(|| {
            let routers: Vec<Vec<NodeId>> = self
                .routers
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.nets())
                .collect();
            Self::routed_nets(&routers, net).into_iter().find_map(on)
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
            // Loopback, so a node can reach a service it hosts via 127.0.0.1 /
            // ::1. On-link here only makes smoltcp emit the frame; the hub loops
            // it back to this same node (see `is_loopback` in `step`).
            let _ = addrs.push(IpCidr::new(Ipv4Address::new(127, 0, 0, 1).into(), 8));
            let _ = addrs.push(IpCidr::new(Ipv6Address::LOCALHOST.into(), 128));
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

    /// Attach a trunk to virtual network `net`: frames on that net with no
    /// local destination flow out of it instead of dropping.
    pub fn attach_trunk(&self, net: NodeId) -> Arc<TrunkPort> {
        let trunk = Arc::new(TrunkPort {
            net: Mutex::new(net),
            outbound: queue(),
            inbound: queue(),
        });
        self.trunks.lock().unwrap().push(trunk.clone());
        trunk
    }

    /// Attach a router bridging `nets`: a frame with no owner on one of them
    /// may be delivered to an owner on another.
    pub fn attach_router(&self, nets: Vec<NodeId>) -> Arc<RouterPort> {
        let router = Arc::new(RouterPort {
            nets: Mutex::new(nets),
        });
        self.routers.lock().unwrap().push(router.clone());
        router
    }

    /// Remove a router (on unwire / node close); the nets it joined are
    /// isolated from each other again.
    pub fn detach_router(&self, router: &Arc<RouterPort>) {
        self.routers
            .lock()
            .unwrap()
            .retain(|r| !Arc::ptr_eq(r, router));
    }

    /// Every net reachable from `net` across routers, `net` itself excluded.
    /// Transitive, so routers chained A-B and B-C put C in A's set — and a set
    /// rather than a walk, so a ring of routers cannot loop a frame.
    fn routed_nets(routers: &[Vec<NodeId>], net: NodeId) -> Vec<NodeId> {
        let mut seen = vec![net];
        let mut i = 0;
        while i < seen.len() {
            let from = seen[i];
            i += 1;
            for r in routers.iter().filter(|r| r.contains(&from)) {
                for &n in r {
                    if !seen.contains(&n) {
                        seen.push(n);
                    }
                }
            }
        }
        seen.remove(0);
        seen
    }

    /// Remove a trunk (on unwire / node close); its net's off-fabric frames
    /// drop again.
    pub fn detach_trunk(&self, trunk: &Arc<TrunkPort>) {
        self.trunks
            .lock()
            .unwrap()
            .retain(|t| !Arc::ptr_eq(t, trunk));
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
                // A frame to 127.0.0.0/8 or ::1 is the node talking to itself:
                // loop it straight back into this NIC rather than onto the net.
                if frame_dst(&frame).is_some_and(is_loopback) {
                    device.deliver(frame);
                } else {
                    outbound.push((net, frame));
                }
            }
            routes.push((net, ip, ip6, s.clone()));
        }

        // Phase 2: deliver each frame to the same-network node owning the dest
        // IP. A stack-originated frame with no local owner leaves through the
        // net's trunk(s) — or drops if there are none (the isolation boundary).
        // Frames a trunk injected deliver to local stacks only (split horizon:
        // an unknown dst must not bounce back out to the remote side).
        let trunks: Vec<Arc<TrunkPort>> = self.trunks.lock().unwrap().clone();
        let routers: Vec<Vec<NodeId>> = self
            .routers
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.nets())
            .collect();
        let deliver = |net: NodeId, frame: Frame, from_trunk: bool| {
            let Some(dst) = frame_dst(&frame) else { return };

            // Multicast: no node owns a group address, so instead of looking
            // for one owner, copy the frame to every member of this network
            // and of anything a router bridges it to.
            //
            // This is what a switch without IGMP snooping does — flood the
            // segment and let each host filter — and here it is the whole of
            // multicast, because the two jobs that make POSIX's
            // IP_ADD_MEMBERSHIP necessary do not exist on the fabric: there is
            // no NIC whose MAC filter must be programmed (the medium is raw IP,
            // with no Ethernet header at all), and no snooping switch to inform
            // by IGMP. What is left is only "which of my members get a copy",
            // and that is the hub's decision to make.
            //
            // Membership is therefore IMPLICIT: a node receives a group it
            // never joined, and its UDP port matching does the filtering a
            // kernel would otherwise have done by group. Within one Network —
            // an isolated segment whose members are wired together on purpose —
            // that is the same exposure they already have to each other's
            // unicast traffic.
            //
            // The sender gets a copy too, which is IP_MULTICAST_LOOP's default
            // and what RTPS discovery expects.
            if is_multicast(dst) {
                let mut nets = vec![net];
                nets.extend(Self::routed_nets(&routers, net));
                for (n, _, _, stack) in routes.iter() {
                    if !nets.contains(n) {
                        continue;
                    }
                    let mut g = stack.lock().unwrap();
                    // smoltcp drops a multicast packet for a group it has not
                    // joined (interface/ipv4.rs: "Ignore IP packets not
                    // directed at us, or broadcast, or any of the multicast
                    // groups"). We own that Interface, so we join on the
                    // node's behalf rather than making the guest ask — which
                    // it could not do anyway: wasi:sockets 0.2 has no
                    // multicast surface. Idempotent, and cheap after the
                    // first frame of a group.
                    let _ = g.iface.join_multicast_group(dst);
                    g.device.deliver(frame.clone());
                }
                // Off to the trunks as well, so a group crosses an uplink the
                // way unicast does. Not `from_trunk`, or a group would echo
                // back to the side it arrived from.
                if !from_trunk {
                    for t in trunks.iter().filter(|t| t.net() == net) {
                        t.deliver_outbound(frame.clone());
                    }
                }
                return;
            }

            let owner = |on: NodeId| {
                routes
                    .iter()
                    .find(|(n, v4, v6, _)| {
                        *n == on && (dst == IpAddress::Ipv4(*v4) || dst == IpAddress::Ipv6(*v6))
                    })
                    .map(|(_, _, _, stack)| stack.clone())
            };
            // Its own net first, then anything a router bridges it to: a local
            // address always wins, so wiring a router can never redirect
            // traffic that was already being delivered.
            if let Some(stack) =
                owner(net).or_else(|| Self::routed_nets(&routers, net).into_iter().find_map(owner))
            {
                stack.lock().unwrap().device.deliver(frame);
            } else if !from_trunk {
                for t in trunks.iter().filter(|t| t.net() == net) {
                    t.deliver_outbound(frame.clone());
                }
            }
        };
        for (net, frame) in outbound {
            deliver(net, frame, false);
        }
        for t in &trunks {
            let net = t.net();
            for frame in t.drain_inbound() {
                deliver(net, frame, true);
            }
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

    /// A v4-only listener (the family-scoped endpoints of `listen_endpoints`)
    /// never sees a v6 connection: a peer dialing the node's fabric ULA on that
    /// port is refused (RST → the dialing socket dies), while a v4 dial to the
    /// same port establishes. This is what keeps a v6 peer address out of a
    /// guest's v4 accept() — wasi-libc aborts on the family mismatch.
    #[test]
    fn v4_only_listeners_refuse_v6_dials() {
        let hub = NetHub::new();
        let net = NodeId::nil();
        let server = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "server");
        let client = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "client");
        let (server_ip, server_ip6) = {
            let g = server.lock().unwrap();
            (g.ip, g.ip6)
        };

        // A v4-bound-to-0.0.0.0 guest socket: one listener per covered local
        // address (fabric v4 + v4 loopback), none matching the ULA.
        {
            let mut g = server.lock().unwrap();
            for ep in g.listen_endpoints(IpFamily::V4, None, 80) {
                let h = g.sockets.add(tcp_socket());
                g.sockets.get_mut::<tcp::Socket>(h).listen(ep).unwrap();
            }
        }

        let dial = |dst: IpAddress, lport: u16| {
            let mut g = client.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            let NodeStack { iface, sockets, .. } = &mut *g;
            sockets
                .get_mut::<tcp::Socket>(h)
                .connect(iface.context(), (dst, 80), lport)
                .unwrap();
            h
        };

        // v6 dial: refused — the SYN to fd00::2 matches no listener, smoltcp
        // answers RST, and the dialing socket falls out of the handshake.
        let h6 = dial(server_ip6.into(), 49152);
        let mut refused = false;
        for _ in 0..500 {
            hub.step();
            let state = client
                .lock()
                .unwrap()
                .sockets
                .get::<tcp::Socket>(h6)
                .state();
            assert_ne!(
                state,
                TcpState::Established,
                "a v6 dial must never land on a v4-only listener"
            );
            if state == TcpState::Closed {
                refused = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(refused, "the v6 dial was refused (RST), not left hanging");

        // v4 dial to the same port: establishes.
        let h4 = dial(server_ip.into(), 49153);
        let mut established = false;
        for _ in 0..500 {
            hub.step();
            if client
                .lock()
                .unwrap()
                .sockets
                .get::<tcp::Socket>(h4)
                .state()
                == TcpState::Established
            {
                established = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(established, "the v4 dial reaches the v4 listener");
    }

    /// Several sockets can listen on one port at once, and concurrent peers each
    /// land on a distinct one — the accept backlog wk-server relies on so a
    /// server doesn't refuse all-but-one simultaneous client.
    #[test]
    fn concurrent_peers_land_on_a_pool_of_listeners() {
        let hub = NetHub::new();
        let net = NodeId::nil();
        let server = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "server");
        let server_ip = server.lock().unwrap().ip;

        // A pool of three listeners on the same port.
        let listeners: Vec<SocketHandle> = (0..3)
            .map(|_| {
                let mut g = server.lock().unwrap();
                let h = g.sockets.add(tcp_socket());
                g.sockets.get_mut::<tcp::Socket>(h).listen(80).unwrap();
                h
            })
            .collect();

        // Three clients dial the server at the same time.
        let clients: Vec<SharedStack> = (0..3)
            .map(|i| hub.attach(net, Ipv4Address::new(10, 0, 0, 2 + i), "client"))
            .collect();
        for (i, c) in clients.iter().enumerate() {
            let mut g = c.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            let NodeStack { iface, sockets, .. } = &mut *g;
            sockets
                .get_mut::<tcp::Socket>(h)
                .connect(iface.context(), (server_ip, 80), 49152 + i as u16)
                .unwrap();
        }

        let mut established = 0;
        for _ in 0..500 {
            hub.step();
            let g = server.lock().unwrap();
            established = listeners
                .iter()
                .filter(|&&h| g.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established)
                .count();
            drop(g);
            if established == 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(established, 3, "all three concurrent peers were accepted");
    }

    /// A single node reaches a service it hosts on `127.0.0.1`: the frame never
    /// leaves the node — the hub loops it back — so a server and a client in one
    /// node connect, the way `localhost` works on a real host.
    #[test]
    fn loopback_within_a_node() {
        let hub = NetHub::new();
        let net = NodeId::nil();
        let node = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "solo");

        let server_h = {
            let mut g = node.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            g.sockets.get_mut::<tcp::Socket>(h).listen(80).unwrap();
            h
        };
        let client_h = {
            let mut g = node.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            let NodeStack { iface, sockets, .. } = &mut *g;
            sockets
                .get_mut::<tcp::Socket>(h)
                .connect(iface.context(), (Ipv4Address::new(127, 0, 0, 1), 80), 49152)
                .unwrap();
            h
        };

        let mut sent = false;
        let mut got: Vec<u8> = Vec::new();
        for _ in 0..500 {
            hub.step();
            {
                let mut g = node.lock().unwrap();
                let cs = g.sockets.get_mut::<tcp::Socket>(client_h);
                if cs.can_send() && !sent {
                    cs.send_slice(b"loopback works").unwrap();
                    sent = true;
                }
            }
            {
                let mut g = node.lock().unwrap();
                let ss = g.sockets.get_mut::<tcp::Socket>(server_h);
                if ss.can_recv() {
                    let mut buf = [0u8; 64];
                    let n = ss.recv_slice(&mut buf).unwrap();
                    got.extend_from_slice(&buf[..n]);
                }
            }
            if got.len() >= 14 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(&got, b"loopback works");
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

    /// One datagram to a group address reaches EVERY member of the network,
    /// including the sender — which is IP_MULTICAST_LOOP's default and what
    /// RTPS discovery expects.
    ///
    /// Nobody joined anything: membership on the fabric is implicit, because
    /// the hub joins the group on each node's smoltcp for it. Without that the
    /// receiving stack silently drops the packet, so this test is really about
    /// the join as much as the routing — a "delivered to all" that forgot it
    /// would pass the send and fail here.
    #[test]
    fn multicast_reaches_every_member_of_the_network() {
        let group = Ipv4Address::new(239, 255, 0, 1);
        let hub = NetHub::new();
        let net = NodeId::nil();
        let sender = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "sender");
        let peer = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "peer");
        // A third node on a DIFFERENT network must not hear it: flooding stops
        // at the segment boundary, exactly as unicast does.
        let other_net = NodeId::from_u128(1);
        let outsider = hub.attach(other_net, Ipv4Address::new(10, 0, 0, 3), "outsider");

        let bind = |stack: &SharedStack| {
            let mut g = stack.lock().unwrap();
            let h = g.sockets.add(udp_socket());
            g.sockets.get_mut::<udp::Socket>(h).bind(7400).unwrap();
            h
        };
        let sender_h = bind(&sender);
        let peer_h = bind(&peer);
        let outsider_h = bind(&outsider);

        let recv = |stack: &SharedStack, h| {
            let mut g = stack.lock().unwrap();
            let s = g.sockets.get_mut::<udp::Socket>(h);
            s.recv().ok().map(|(d, _)| d.to_vec())
        };

        let mut sent = false;
        let (mut at_peer, mut at_sender) = (None, None);
        for _ in 0..500 {
            hub.step();
            if !sent {
                let mut g = sender.lock().unwrap();
                let s = g.sockets.get_mut::<udp::Socket>(sender_h);
                if s.can_send() {
                    s.send_slice(b"spdp", (group, 7400)).unwrap();
                    sent = true;
                }
            }
            if at_peer.is_none() {
                at_peer = recv(&peer, peer_h);
            }
            if at_sender.is_none() {
                at_sender = recv(&sender, sender_h);
            }
            if at_peer.is_some() && at_sender.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(at_peer.as_deref(), Some(&b"spdp"[..]), "peer on the net");
        assert_eq!(
            at_sender.as_deref(),
            Some(&b"spdp"[..]),
            "sender loops back"
        );
        assert_eq!(
            recv(&outsider, outsider_h),
            None,
            "a node on another network must not hear the group"
        );
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

    /// Two independent fabrics (separate hubs, as in two wk instances) joined
    /// by a trunk on each and a "virtual cable" shuttling frames between them:
    /// a TCP client on one fabric reaches a server on the other. This is the
    /// packet path an Iroh uplink node rides — the cable becomes the QUIC
    /// connection.
    #[test]
    fn trunked_fabrics_talk_tcp_across_hubs() {
        let hub_a = NetHub::new();
        let hub_b = NetHub::new();
        let net = NodeId::nil();
        let client = hub_a.attach(net, Ipv4Address::new(10, 0, 0, 1), "client");
        let server = hub_b.attach(net, Ipv4Address::new(10, 0, 0, 2), "server");
        let trunk_a = hub_a.attach_trunk(net);
        let trunk_b = hub_b.attach_trunk(net);

        // The cable: shuttle frames both ways (an uplink node's pump).
        let (ta, tb) = (trunk_a.clone(), trunk_b.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let cable_stop = stop.clone();
        let cable = std::thread::spawn(move || {
            while !cable_stop.load(Ordering::Relaxed) {
                for f in ta.drain_outbound() {
                    tb.inject(f);
                }
                for f in tb.drain_outbound() {
                    ta.inject(f);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });

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
                .connect(iface.context(), (Ipv4Address::new(10, 0, 0, 2), 80), 49152)
                .unwrap();
            h
        };

        let mut sent = false;
        let mut got: Vec<u8> = Vec::new();
        for _ in 0..500 {
            hub_a.step();
            hub_b.step();
            {
                let mut g = client.lock().unwrap();
                let cs = g.sockets.get_mut::<tcp::Socket>(client_h);
                if cs.can_send() && !sent {
                    cs.send_slice(b"across fabrics").unwrap();
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
            if got.len() >= 14 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        stop.store(true, Ordering::Relaxed);
        cable.join().unwrap();
        assert_eq!(&got, b"across fabrics");
    }

    /// A trunk only taps its own network: another net's off-fabric frames still
    /// drop, and frames a trunk injected never leave through a trunk again
    /// (split horizon), so joined fabrics can't loop unknown destinations.
    #[test]
    fn trunk_taps_its_net_only_and_never_reflects() {
        let hub = NetHub::new();
        let net = NodeId::nil();
        let other_net = NodeId::new();
        let sender = hub.attach(other_net, Ipv4Address::new(10, 0, 0, 1), "sender");
        let trunk = hub.attach_trunk(net);
        let trunk2 = hub.attach_trunk(net);

        // A node on ANOTHER net sends to an address nobody owns.
        let h = {
            let mut g = sender.lock().unwrap();
            let h = g.sockets.add(udp_socket());
            g.sockets.get_mut::<udp::Socket>(h).bind(4000).unwrap();
            h
        };
        sender
            .lock()
            .unwrap()
            .sockets
            .get_mut::<udp::Socket>(h)
            .send_slice(b"lost", (Ipv4Address::new(10, 0, 0, 99), 4001))
            .unwrap();
        for _ in 0..10 {
            hub.step();
        }
        assert!(
            trunk.drain_outbound().is_empty(),
            "trunk tapped a frame from a different net"
        );

        // Move the sender onto the trunked net: now the frame flows out — and
        // to BOTH trunks on the net.
        sender.lock().unwrap().net = net;
        sender
            .lock()
            .unwrap()
            .sockets
            .get_mut::<udp::Socket>(h)
            .send_slice(b"tapped", (Ipv4Address::new(10, 0, 0, 99), 4001))
            .unwrap();
        let mut out = Vec::new();
        for _ in 0..10 {
            hub.step();
            out.extend(trunk.drain_outbound());
        }
        assert!(
            !out.is_empty(),
            "trunk missed an off-fabric frame on its net"
        );
        let mut out2 = Vec::new();
        for _ in 0..2 {
            out2.extend(trunk2.drain_outbound());
        }
        assert!(!out2.is_empty(), "second trunk on the net missed the frame");

        // Re-injecting that unknown-dst frame must NOT come back out of any
        // trunk (split horizon) — it just drops.
        trunk.inject(out[0].clone());
        for _ in 0..10 {
            hub.step();
        }
        assert!(trunk.drain_outbound().is_empty());
        assert!(trunk2.drain_outbound().is_empty());
    }

    /// Nodes on DIFFERENT virtual networks can't reach each other, even at the
    /// same address — the isolation boundary (off-network packets are dropped).
    #[test]
    fn a_router_bridges_two_networks_and_only_those() {
        // The same shape as `different_networks_are_isolated` — a client and a
        // server on separate nets — except a router joins the two. Bridging is
        // permission, not translation: the server keeps its own address.
        let server_ip = Ipv4Address::new(10, 0, 0, 2);
        let hub = NetHub::new();
        let (net_a, net_b, net_c) = (NodeId::new(), NodeId::new(), NodeId::new());
        let client = hub.attach(net_a, Ipv4Address::new(10, 0, 0, 1), "client");
        let server = hub.attach(net_b, server_ip, "server");
        // A third net the router does not name stays sealed off (see below).
        let outsider = hub.attach(net_c, Ipv4Address::new(10, 0, 0, 3), "server");
        let _router = hub.attach_router(vec![net_a, net_b]);

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
        assert_eq!(
            client
                .lock()
                .unwrap()
                .sockets
                .get::<tcp::Socket>(client_h)
                .state(),
            tcp::State::Established,
            "the router should have carried the handshake across"
        );
        assert!(server
            .lock()
            .unwrap()
            .sockets
            .get::<tcp::Socket>(server_h)
            .is_active());

        // Fabric DNS follows the bridge, and the net the router never named is
        // not reachable by name either — both "server" nodes exist, and the
        // client resolves the one across its own router.
        assert_eq!(hub.resolve(net_a, "server"), Some(server_ip));
        assert_eq!(hub.resolve(net_c, "client"), None, "net C is not bridged");
        drop(outsider);
    }

    /// A name on your own net always wins, so wiring a router cannot silently
    /// redirect a name that already resolved — the property that keeps two
    /// instances of one definition from stealing each other's traffic.
    #[test]
    fn a_local_name_shadows_one_across_a_router() {
        let hub = NetHub::new();
        let (mine, theirs) = (NodeId::new(), NodeId::new());
        let local = Ipv4Address::new(10, 0, 0, 7);
        let _near = hub.attach(mine, local, "python");
        let _far = hub.attach(theirs, Ipv4Address::new(10, 0, 0, 8), "python");
        let _router = hub.attach_router(vec![mine, theirs]);
        assert_eq!(hub.resolve(mine, "python"), Some(local));
    }

    /// Detaching a router re-seals the nets it joined: the bridge is live
    /// state, so unwiring one in the UI has to take effect immediately.
    #[test]
    fn detaching_a_router_reseals_the_nets() {
        let hub = NetHub::new();
        let (a, b) = (NodeId::new(), NodeId::new());
        let _client = hub.attach(a, Ipv4Address::new(10, 0, 0, 1), "client");
        let server_ip = Ipv4Address::new(10, 0, 0, 2);
        let _server = hub.attach(b, server_ip, "server");
        let router = hub.attach_router(vec![a, b]);
        assert_eq!(hub.resolve(a, "server"), Some(server_ip));
        hub.detach_router(&router);
        assert_eq!(hub.resolve(a, "server"), None);
    }

    /// Routers chained A-B and B-C put C within reach of A, and a ring of them
    /// terminates rather than looping a frame forever.
    #[test]
    fn routed_nets_are_transitive_and_ring_safe() {
        let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
        let chain = vec![vec![a, b], vec![b, c]];
        let mut reach = NetHub::routed_nets(&chain, a);
        reach.sort();
        let mut want = vec![b, c];
        want.sort();
        assert_eq!(reach, want, "A reaches C through B");

        let ring = vec![vec![a, b], vec![b, c], vec![c, a]];
        assert_eq!(NetHub::routed_nets(&ring, a).len(), 2, "each net once");
    }

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
