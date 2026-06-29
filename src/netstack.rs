//! wk's userspace network fabric.
//!
//! The vision: wk owns the network the way it owns the filesystem (the vfs).
//! Each networked node gets a virtual NIC on a wk-managed network and its
//! `wasi:sockets` activity terminates in a wk-owned smoltcp stack that emits
//! real IP packets. wk then *routes those packets per the canvas graph* — so
//! wired nodes can reach each other (Docker-bridge style) while unwired nodes
//! see nothing, traffic can be sent out to the host only through an explicit
//! gateway, and packets can be transparently rerouted through middlebox nodes
//! (a VPN, a proxy, a firewall) with zero changes to the wasm client. Because we
//! move *packets*, a "VPN node" can just sit in the path and work.
//!
//! This module is the foundation: the virtual NIC (a smoltcp [`phy::Device`])
//! and the [`Fabric`] switch that routes IPv4 packets between NICs by
//! destination address. Higher layers (our own `wasi:sockets` host impl, the
//! per-node stacks, canvas wiring, gateway, middleboxes) build on this.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use smoltcp::phy::{self, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{Ipv4Address, Ipv4Packet};

/// One raw IP packet on the fabric (Medium::Ip — no Ethernet header).
pub type Frame = Vec<u8>;

/// A shared frame queue between a NIC and the fabric.
type Queue = Arc<Mutex<VecDeque<Frame>>>;

fn queue() -> Queue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// A node's virtual network interface: a smoltcp device whose transmitted
/// packets go to the fabric (`tx`) and whose received packets come from it
/// (`rx`). The matching ends live in the [`Fabric`].
pub struct VirtualNic {
    /// Packets the fabric has delivered to this node, awaiting `receive`.
    rx: Queue,
    /// Packets this node has transmitted, awaiting routing by the fabric.
    tx: Queue,
}

impl Device for VirtualNic {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = 65535;
        // A virtual link can't corrupt bits, so don't spend cycles on checksums.
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

/// A port on the fabric: a node's address and the two queues connecting it.
struct Port {
    addr: Ipv4Address,
    /// Fabric -> node (this NIC's `rx`).
    to_node: Queue,
    /// Node -> fabric (this NIC's `tx`).
    from_node: Queue,
}

/// A wk-managed virtual network: a switch that routes IPv4 packets between the
/// NICs attached to it, by destination address (a packet to an address with no
/// port on this fabric is dropped). This is one isolated network segment — only
/// nodes attached here can reach each other.
#[derive(Default)]
pub struct Fabric {
    ports: Vec<Port>,
}

impl Fabric {
    pub fn new() -> Self {
        Fabric::default()
    }

    /// Attach a node at `addr`, returning the NIC it should drive its smoltcp
    /// interface with.
    pub fn attach(&mut self, addr: Ipv4Address) -> VirtualNic {
        let rx = queue();
        let tx = queue();
        self.ports.push(Port {
            addr,
            to_node: rx.clone(),
            from_node: tx.clone(),
        });
        VirtualNic { rx, tx }
    }

    /// Move every transmitted packet to its destination node's receive queue.
    /// Packets addressed off-fabric (no matching port) are dropped — that's the
    /// isolation boundary; reaching elsewhere requires an explicit gateway.
    pub fn route(&self) {
        // Collect first so we don't hold a queue lock while taking another.
        let mut pending: Vec<Frame> = Vec::new();
        for port in &self.ports {
            let mut out = port.from_node.lock().unwrap();
            pending.extend(out.drain(..));
        }
        for frame in pending {
            let Ok(pkt) = Ipv4Packet::new_checked(&frame[..]) else {
                continue;
            };
            let dst = pkt.dst_addr();
            if let Some(port) = self.ports.iter().find(|p| p.addr == dst) {
                port.to_node.lock().unwrap().push_back(frame);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::socket::tcp;
    use smoltcp::wire::{HardwareAddress, IpCidr};

    fn iface_for(nic: &mut VirtualNic, addr: Ipv4Address) -> Interface {
        let config = Config::new(HardwareAddress::Ip);
        let mut iface = Interface::new(config, nic, Instant::ZERO);
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(addr.into(), 24)).unwrap();
        });
        iface
    }

    fn tcp_socket() -> tcp::Socket<'static> {
        tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 4096]),
            tcp::SocketBuffer::new(vec![0u8; 4096]),
        )
    }

    /// Two independent smoltcp stacks on one fabric exchange a TCP stream — the
    /// whole foundation (virtual NIC + switch routing) end to end.
    #[test]
    fn two_nodes_talk_tcp_over_the_fabric() {
        let server_ip = Ipv4Address::new(10, 0, 0, 2);
        let mut fabric = Fabric::new();
        let mut client_nic = fabric.attach(Ipv4Address::new(10, 0, 0, 1));
        let mut server_nic = fabric.attach(server_ip);
        let mut client_if = iface_for(&mut client_nic, Ipv4Address::new(10, 0, 0, 1));
        let mut server_if = iface_for(&mut server_nic, server_ip);

        let mut client_socks = SocketSet::new(vec![]);
        let mut server_socks = SocketSet::new(vec![]);
        let server_h = server_socks.add(tcp_socket());
        server_socks
            .get_mut::<tcp::Socket>(server_h)
            .listen(80)
            .unwrap();
        let client_h = client_socks.add(tcp_socket());
        client_socks
            .get_mut::<tcp::Socket>(client_h)
            .connect(client_if.context(), (server_ip, 80), 49152)
            .unwrap();

        let mut clock = 0i64;
        let mut sent = false;
        let mut got: Vec<u8> = Vec::new();
        for _ in 0..2000 {
            let t = Instant::from_millis(clock);
            client_if.poll(t, &mut client_nic, &mut client_socks);
            server_if.poll(t, &mut server_nic, &mut server_socks);
            fabric.route();

            let cs = client_socks.get_mut::<tcp::Socket>(client_h);
            if cs.can_send() && !sent {
                cs.send_slice(b"hello wk net").unwrap();
                sent = true;
            }
            let ss = server_socks.get_mut::<tcp::Socket>(server_h);
            if ss.can_recv() {
                let mut buf = [0u8; 64];
                let n = ss.recv_slice(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            if got.len() >= 12 {
                break;
            }
            clock += 5;
        }
        assert_eq!(&got, b"hello wk net", "server received the client's bytes");
    }
}
