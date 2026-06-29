//! wk's userspace network fabric.
//!
//! wk owns the network the way it owns the filesystem (the vfs). Each networked
//! node gets a virtual NIC + its own smoltcp stack; its `wasi:sockets` activity
//! (see [`crate::sockets`]) terminates there and emits real IP packets. A single
//! background hub thread drives every node's stack and routes packets between
//! nodes **on the same virtual network** — so wired nodes reach each other
//! (Docker-bridge style) and unwired nodes (alone on their own network) see
//! nothing. Because we move *packets*, traffic can later be rerouted through
//! middlebox nodes (a VPN/proxy) transparently to the wasm client.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{self, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpCidr, Ipv4Address, Ipv4Packet};

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
    /// Deliver a frame to this NIC's receive queue.
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
    pub net: u64,
    pub ip: Ipv4Address,
    /// Whether this node may reach the real host network (set when wired to a
    /// Gateway node). Off-fabric connections are bridged to host sockets.
    pub host_access: bool,
    /// Wakers parked on this stack's pollables; woken each hub tick so guest
    /// socket pollables re-check readiness.
    wakers: Vec<Waker>,
}

impl NodeStack {
    /// Park a waker to be woken on the next hub tick (state may have changed).
    pub fn park(&mut self, w: Waker) {
        self.wakers.push(w);
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

    /// Attach a node to virtual network `net` at address `ip`, returning its
    /// stack (to drive via wasi:sockets).
    pub fn attach(&self, net: u64, ip: Ipv4Address) -> SharedStack {
        let mut device = VirtualNic::new();
        let config = Config::new(HardwareAddress::Ip);
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(ip.into(), 24));
        });
        let stack = Arc::new(Mutex::new(NodeStack {
            iface,
            sockets: SocketSet::new(Vec::new()),
            device,
            net,
            ip,
            host_access: false,
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
        let mut outbound: Vec<(u64, Frame)> = Vec::new();
        // Snapshot (net, ip, stack) for delivery lookup.
        let mut routes: Vec<(u64, Ipv4Address, SharedStack)> = Vec::new();
        for s in &stacks {
            let mut g = s.lock().unwrap();
            let NodeStack {
                iface,
                sockets,
                device,
                net,
                ip,
                ..
            } = &mut *g;
            iface.poll(now, device, sockets);
            let net = *net;
            let ip = *ip;
            for frame in device.drain_tx() {
                outbound.push((net, frame));
            }
            routes.push((net, ip, s.clone()));
        }

        // Phase 2: deliver each frame to the same-network node owning the dest IP.
        for (net, frame) in outbound {
            let Ok(pkt) = Ipv4Packet::new_checked(&frame[..]) else {
                continue;
            };
            let dst = pkt.dst_addr();
            if let Some((_, _, stack)) = routes.iter().find(|(n, ip, _)| *n == net && *ip == dst) {
                stack.lock().unwrap().device.deliver(frame);
            }
            // Off-network / unknown dest: dropped (the isolation boundary).
        }

        // Phase 3: poll again so delivered frames are processed now, and wake
        // pollables so guest socket operations re-check.
        for s in &stacks {
            let mut g = s.lock().unwrap();
            let NodeStack {
                iface,
                sockets,
                device,
                ..
            } = &mut *g;
            iface.poll(now, device, sockets);
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
    use smoltcp::socket::tcp;

    fn tcp_socket() -> tcp::Socket<'static> {
        tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 4096]),
            tcp::SocketBuffer::new(vec![0u8; 4096]),
        )
    }

    /// Two nodes on the same virtual network exchange a TCP stream, driven by the
    /// hub's `step` — exercises the NIC, the per-network routing, and the stacks.
    #[test]
    fn same_network_nodes_talk_tcp() {
        let server_ip = Ipv4Address::new(10, 0, 0, 2);
        let hub = NetHub::new();
        let client = hub.attach(1, Ipv4Address::new(10, 0, 0, 1));
        let server = hub.attach(1, server_ip);

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
        let client = hub.attach(1, Ipv4Address::new(10, 0, 0, 1)); // net 1
        let server = hub.attach(2, server_ip); // net 2 — isolated

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
