//! The **host multicast bridge**: joins a virtual Network's multicast domain
//! to the host's real one, so a group a fabric node sends to also leaves the
//! machine, and a group a real peer sends to arrives on the fabric.
//!
//! This is a [`TrunkPort`] like [`crate::uplink`] is, and for the same reason:
//! a trunk receives every frame its net has no local owner for, and
//! `NetHub::step` copies multicast to the trunks as well as to the net's
//! members. The difference is what sits on the far side. An iroh uplink's
//! remote is another fabric, so it forwards frames *verbatim*. Here the remote
//! is a real network that has never heard of `10.0.0.0/24`, so the bridge
//! translates instead: it reads the datagram out of the frame, sends it from a
//! real socket, and builds a fresh frame around each datagram coming back.
//!
//! WHAT IS BRIDGED, AND WHAT IS NOT
//! ================================
//! **Multicast UDP, both ways.** Only multicast. Unicast off the fabric
//! already has a path — a node wired to a Gateway sends off-fabric datagrams
//! straight out a host socket (see `wk-server`'s `sockets.rs`) — and multicast
//! was the hole: a group address is fabric traffic by definition there, so it
//! reached every member of the Network and stopped at the machine's edge.
//!
//! **Addresses inside payloads are not touched.** The bridge is a layer-3
//! translator, not an application gateway. A protocol that announces its own
//! address *in* the datagram (RTPS discovery is the case in hand: SPDP carries
//! the participant's unicast locators) still announces its fabric address,
//! which a real peer cannot route to. So a real peer hears a fabric
//! participant and can be heard by it, but cannot open the unicast follow-up
//! back. Rewriting locators is an RTPS-aware job and belongs above this layer.
//!
//! WHERE THE GROUPS COME FROM
//! ==========================
//! Nobody configures them. Membership on the fabric is implicit (see the
//! multicast branch of `NetHub::step`) — a node sends to a group without
//! joining it, so there is no join for the bridge to observe. What there is,
//! is traffic: the first frame a Network sends to `239.255.0.1:7400` tells the
//! bridge that this Network cares about that group on that port, and it joins
//! it on the host from then on. A group the Network never uses is never joined,
//! which keeps the bridge from pulling in every group on the LAN.
//!
//! The cost is that the fabric has to speak first. A Network that only ever
//! *listens* to a group joins nothing, because nothing on the fabric can say
//! it wants to — so a bridge can also be given groups up front (`groups` in
//! [`HostMulticast::start`]) for that case.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr, IPV4_HEADER_LEN,
};
use wk_protocol::NodeId;

use crate::netstack::{Frame, NetHub, TrunkPort};

/// The hop budget on datagrams the bridge sends out. 1 would confine them to
/// the local segment, which is the safe default for multicast but would make a
/// bridge useless across a router; 4 is enough for a small routed site and
/// still far from flooding anything.
const MCAST_TTL: u32 = 4;

/// A group the bridge carries: the address and the port it was seen on.
///
/// The port is part of the identity because a receive socket is bound to one
/// port. Two protocols sharing a group on different ports are two entries, and
/// each gets its own socket — which is also what keeps the bridge from
/// delivering one protocol's datagrams to the other's port.
pub type Group = (Ipv4Address, u16);

/// A running host multicast bridge: a trunk on one Network whose far side is
/// the host's real network. Dropping it detaches the trunk, stops the pump and
/// leaves every group it joined.
pub struct HostMulticast {
    trunk: Arc<TrunkPort>,
    hub: Arc<NetHub>,
    /// The groups currently joined on the host, for `wk ps` to show — a bridge
    /// that has joined nothing is the visible symptom of a Network that has
    /// not sent to a group yet.
    joined: Arc<Mutex<Vec<Group>>>,
    kill: Arc<AtomicBool>,
}

impl HostMulticast {
    /// Bridge network `net`'s multicast to the host. `iface` is the local
    /// address of the interface to send and receive on; `None` lets the kernel
    /// pick (the default route's interface), which is what you want unless the
    /// machine is multi-homed. `groups` are joined immediately, for a Network
    /// that only listens; anything the Network sends to is joined as it is
    /// seen.
    pub fn start(
        hub: Arc<NetHub>,
        net: NodeId,
        iface: Option<Ipv4Addr>,
        groups: &[Group],
    ) -> Result<HostMulticast> {
        // One socket sends every group. Its source port is what identifies our
        // own looped-back datagrams below, so bind before anything can send.
        let egress = egress_socket(iface).context("bind the multicast bridge's send socket")?;
        let egress_port = egress.local_addr()?.port();

        // Join the caller's groups HERE rather than in the pump, so a group
        // that cannot be joined — a port already taken by something that did
        // not ask for SO_REUSEPORT, a bad interface — is an error the caller
        // sees, not a silence inside a thread. Groups learned from traffic
        // later can only be dropped, because there is nobody left to tell.
        let iface_addr = iface.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let mut rx: HashMap<Group, UdpSocket> = HashMap::new();
        for &(addr, port) in groups {
            let sock = bind_group(Ipv4Addr::from(addr.octets()), port, iface_addr)
                .with_context(|| format!("join {addr} on port {port}"))?;
            rx.insert((addr, port), sock);
        }

        let trunk = hub.attach_trunk(net);
        let joined: Arc<Mutex<Vec<Group>>> = Arc::new(Mutex::new(rx.keys().copied().collect()));
        let kill = Arc::new(AtomicBool::new(false));

        let (t, j, k) = (trunk.clone(), joined.clone(), kill.clone());
        std::thread::Builder::new()
            .name("wk-hostmcast".into())
            .spawn(move || pump(t, j, k, egress, egress_port, iface_addr, rx))
            .context("spawn the multicast bridge thread")?;

        Ok(HostMulticast {
            trunk,
            hub,
            joined,
            kill,
        })
    }

    /// Move the bridge to another network (the trunk follows the wire).
    pub fn set_net(&self, net: NodeId) {
        self.trunk.set_net(net);
    }

    /// The groups joined on the host so far.
    pub fn joined(&self) -> Vec<Group> {
        self.joined.lock().unwrap().clone()
    }
}

impl Drop for HostMulticast {
    fn drop(&mut self) {
        self.kill.store(true, Ordering::Relaxed);
        self.hub.detach_trunk(&self.trunk);
    }
}

/// Shuttle datagrams between the trunk and the host, joining groups as the
/// Network reveals them.
#[allow(clippy::too_many_arguments)]
fn pump(
    trunk: Arc<TrunkPort>,
    joined: Arc<Mutex<Vec<Group>>>,
    kill: Arc<AtomicBool>,
    egress: UdpSocket,
    egress_port: u16,
    iface: Ipv4Addr,
    mut rx: HashMap<Group, UdpSocket>,
) {
    // The fabric MTU is 1280, so nothing the fabric sends is bigger; inbound
    // can be, and a datagram too big for the fabric is truncated rather than
    // dropped silently... which would be a lie. Read into a full-size buffer
    // and drop the oversized ones explicitly instead.
    let mut buf = [0u8; 65536];

    while !kill.load(Ordering::Relaxed) {
        let mut idle = true;

        // Fabric -> host.
        for frame in trunk.drain_outbound() {
            let Some(dg) = parse_udp4(&frame) else {
                continue;
            };
            if !is_mcast4(dg.dst) {
                // Unicast reached the trunk because no node on the Network owns
                // it. That is the isolation boundary doing its job, and not
                // this bridge's business — a node that should reach the host
                // network directly is wired to a Gateway.
                continue;
            }
            idle = false;
            join(&mut rx, &joined, (dg.dst, dg.dport), iface);
            let to = SocketAddrV4::new(Ipv4Addr::from(dg.dst.octets()), dg.dport);
            let _ = egress.send_to(dg.payload, to);
        }

        // Host -> fabric.
        for (&(group, port), sock) in rx.iter() {
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                // Our own datagram, looped back because IP_MULTICAST_LOOP is
                // on. Injecting it would hand every node on the Network a
                // second copy of what it just saw. It cannot loop forever — an
                // injected frame is `from_trunk` and never re-trunked — so the
                // failure this guards against is duplication, not a storm.
                //
                // The test is the source port: only our egress socket has it.
                // A real peer that happened to pick the same ephemeral port
                // would have its datagrams dropped, which is the mild half of
                // the trade against duplicating every packet we send.
                if from.port() == egress_port {
                    continue;
                }
                let std::net::IpAddr::V4(src) = from.ip() else {
                    continue; // a v4 group cannot carry a v6 source
                };
                idle = false;
                if let Some(frame) = build_udp4(
                    Ipv4Address::from(src.octets()),
                    from.port(),
                    group,
                    port,
                    &buf[..n],
                ) {
                    trunk.inject(frame);
                }
            }
        }

        if idle {
            // Matching the hub's own cadence: a datagram waits at most a
            // millisecond in either direction.
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Join `group` on the host if it isn't joined already. A failure is recorded
/// as "not joined" rather than retried in a tight loop — the next frame to the
/// group tries again, which is the retry.
fn join(
    rx: &mut HashMap<Group, UdpSocket>,
    joined: &Arc<Mutex<Vec<Group>>>,
    group: Group,
    iface: Ipv4Addr,
) {
    if rx.contains_key(&group) {
        return;
    }
    let (addr, port) = group;
    let Ok(sock) = bind_group(Ipv4Addr::from(addr.octets()), port, iface) else {
        return;
    };
    rx.insert(group, sock);
    joined.lock().unwrap().push(group);
}

/// The one socket every group is sent from. Its ephemeral source port is how
/// the pump recognises its own looped-back datagrams, so the caller reads the
/// port back before anything can send.
fn egress_socket(iface: Option<Ipv4Addr>) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))?;
    s.set_multicast_ttl_v4(MCAST_TTL)?;
    // Loop stays ON, deliberately. It governs whether *this host's* other
    // sockets see what we send, so switching it off would hide the fabric from
    // a peer running on the same machine — which is the first thing anyone
    // will try. The price is that our own receive sockets get a copy of
    // everything we send, and the pump has to recognise and drop it.
    s.set_multicast_loop_v4(true)?;
    if let Some(addr) = iface {
        s.set_multicast_if_v4(&addr)?;
    }
    s.set_nonblocking(true)?;
    Ok(s.into())
}

/// A socket bound to a group's port and joined to the group.
///
/// `SO_REUSEADDR`/`SO_REUSEPORT` are what let this coexist with another
/// process on the same machine listening to the same group and port — the
/// same-host peer that [`HostMulticast::start`] keeps multicast loop on for.
/// Without them the bind fails outright the moment anything else is listening,
/// and the bridge would be one-way for no visible reason.
fn bind_group(group: Ipv4Addr, port: u16, iface: Ipv4Addr) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    // Bind to the wildcard, not to the group: binding the group address works
    // on Linux and fails on macOS/BSD, and the wildcard is portable. Port
    // matching plus the membership below is what filters, as it would for any
    // multicast receiver.
    s.bind(&SockAddr::from(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        port,
    )))?;
    s.join_multicast_v4(&group, &iface)?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

/// Is this an IPv4 group address (`224.0.0.0/4`)?
fn is_mcast4(addr: Ipv4Address) -> bool {
    addr.octets()[0] & 0xf0 == 0xe0
}

/// The parts of a UDP-over-IPv4 frame the bridge needs.
struct Datagram<'a> {
    dst: Ipv4Address,
    dport: u16,
    payload: &'a [u8],
}

/// Pull the UDP datagram out of a raw fabric frame, or `None` if it isn't
/// UDP-over-IPv4 (TCP, ICMP and IPv6 all reach a trunk too).
fn parse_udp4(frame: &[u8]) -> Option<Datagram<'_>> {
    let ip = Ipv4Packet::new_checked(frame).ok()?;
    if ip.next_header() != IpProtocol::Udp {
        return None;
    }
    let dst = ip.dst_addr();
    // `new_checked` on the payload validates the UDP length field against what
    // is actually there, so a truncated frame stops here.
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    let dport = udp.dst_port();
    // Reborrow from `frame`: the packet views borrow it, and the payload slice
    // has to outlive them.
    let off = IPV4_HEADER_LEN + 8;
    let len = udp.len() as usize;
    let end = off.checked_add(len.checked_sub(8)?)?;
    Some(Datagram {
        dst,
        dport,
        payload: frame.get(off..end)?,
    })
}

/// Wrap a datagram from the host in a fabric frame. `None` if it is too big for
/// the fabric MTU to carry — the same fate a large datagram meets on any path
/// with a smaller MTU downstream.
fn build_udp4(
    src: Ipv4Address,
    sport: u16,
    dst: Ipv4Address,
    dport: u16,
    payload: &[u8],
) -> Option<Frame> {
    if payload.len() + IPV4_HEADER_LEN + 8 > 1280 {
        return None;
    }
    let udp = UdpRepr {
        src_port: sport,
        dst_port: dport,
    };
    let ip = Ipv4Repr {
        src_addr: src,
        dst_addr: dst,
        next_header: IpProtocol::Udp,
        payload_len: 8 + payload.len(),
        hop_limit: 64,
    };
    let caps = ChecksumCapabilities::default();
    let mut frame = vec![0u8; ip.buffer_len() + ip.payload_len];
    let mut pkt = Ipv4Packet::new_unchecked(&mut frame[..]);
    ip.emit(&mut pkt, &caps);
    let mut upkt = UdpPacket::new_unchecked(pkt.payload_mut());
    udp.emit(
        &mut upkt,
        &src.into(),
        &dst.into(),
        payload.len(),
        |b| b.copy_from_slice(payload),
        &caps,
    );
    Some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netstack::SharedStack;
    use smoltcp::socket::udp;

    fn udp_socket() -> udp::Socket<'static> {
        let buf =
            || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 16 * 1280]);
        udp::Socket::new(buf(), buf())
    }

    /// A real host peer on the group: joined so it hears the bridge, and set to
    /// send on the same interface so the bridge hears it.
    fn host_peer(group: Ipv4Addr, port: u16, iface: Ipv4Addr) -> UdpSocket {
        use socket2::{Domain, Protocol, SockAddr, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        s.set_reuse_address(true).unwrap();
        s.set_reuse_port(true).unwrap();
        s.bind(&SockAddr::from(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            port,
        )))
        .unwrap();
        s.join_multicast_v4(&group, &iface).unwrap();
        s.set_multicast_if_v4(&iface).unwrap();
        s.set_multicast_loop_v4(true).unwrap();
        s.set_multicast_ttl_v4(1).unwrap();
        s.set_nonblocking(true).unwrap();
        s.into()
    }

    /// The whole bridge against real host sockets, both directions.
    ///
    /// On the **loopback** interface deliberately: the test needs no LAN, and —
    /// more to the point — cannot put multicast onto one it happens to find.
    /// macOS and Linux both carry multicast on `lo0` when the sender sets
    /// `IP_MULTICAST_IF` to `127.0.0.1` and the receiver joins there, which is
    /// what makes a hermetic test of a LAN feature possible at all.
    #[test]
    fn a_group_crosses_the_host_boundary_both_ways() {
        let lo = Ipv4Addr::new(127, 0, 0, 1);
        let group4 = Ipv4Address::new(239, 255, 99, 8);
        let group = Ipv4Addr::new(239, 255, 99, 8);
        let port = 17401;

        let hub = NetHub::new();
        let net = NodeId::nil();
        let node = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "node");
        let node_h = {
            let mut g = node.lock().unwrap();
            let h = g.sockets.add(udp_socket());
            g.sockets.get_mut::<udp::Socket>(h).bind(port).unwrap();
            h
        };

        // Pre-joined, which is the "a Network that only listens" case: nothing
        // on the fabric has sent to the group yet, so there is nothing for the
        // bridge to have learned from.
        let bridge = HostMulticast::start(hub.clone(), net, Some(lo), &[(group4, port)]).unwrap();
        assert_eq!(bridge.joined(), vec![(group4, port)]);

        let peer = host_peer(group, port, lo);

        // Host -> fabric.
        let mut at_node: Vec<Vec<u8>> = Vec::new();
        let mut at_peer: Vec<Vec<u8>> = Vec::new();
        let mut buf = [0u8; 2048];
        let drain_node = |acc: &mut Vec<Vec<u8>>, stack: &SharedStack| {
            let mut g = stack.lock().unwrap();
            while let Ok((d, _)) = g.sockets.get_mut::<udp::Socket>(node_h).recv() {
                acc.push(d.to_vec());
            }
        };

        peer.send_to(b"from-host", SocketAddrV4::new(group, port))
            .unwrap();
        for _ in 0..3000 {
            hub.step();
            drain_node(&mut at_node, &node);
            if at_node.iter().any(|d| d == b"from-host") {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            at_node.iter().any(|d| d == b"from-host"),
            "a real peer's group datagram never reached the fabric node (got {at_node:?})"
        );

        // Fabric -> host.
        at_node.clear();
        {
            let mut g = node.lock().unwrap();
            let s = g.sockets.get_mut::<udp::Socket>(node_h);
            assert!(s.can_send());
            s.send_slice(b"from-fabric", (group4, port)).unwrap();
        }
        for _ in 0..3000 {
            hub.step();
            drain_node(&mut at_node, &node);
            while let Ok((n, _)) = peer.recv_from(&mut buf) {
                at_peer.push(buf[..n].to_vec());
            }
            if at_peer.iter().any(|d| d == b"from-fabric") {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            at_peer.iter().any(|d| d == b"from-fabric"),
            "the fabric node's group datagram never left the host (got {at_peer:?})"
        );

        // And the bridge did not feed our own datagram back in. The node gets
        // exactly one copy — its own multicast loopback — not a second one
        // round-tripped through the host. This is the assertion that fails if
        // the source-port check in the pump is wrong or missing.
        let mut settle = 0;
        while settle < 300 {
            hub.step();
            drain_node(&mut at_node, &node);
            settle += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        let n = at_node.iter().filter(|d| *d == b"from-fabric").count();
        assert_eq!(
            n, 1,
            "one send, one delivery — the host loop copy leaked in"
        );
    }

    /// A frame the bridge built parses back to the datagram it was given —
    /// the round trip both directions of the pump depend on.
    #[test]
    fn a_built_frame_parses_back() {
        let group = Ipv4Address::new(239, 255, 0, 1);
        let frame = build_udp4(
            Ipv4Address::new(192, 168, 1, 50),
            41234,
            group,
            7400,
            b"spdp announcement",
        )
        .expect("small enough for the fabric");

        let dg = parse_udp4(&frame).expect("valid UDP over IPv4");
        assert_eq!(dg.dst, group);
        assert_eq!(dg.dport, 7400);
        assert_eq!(dg.payload, b"spdp announcement");
        assert!(is_mcast4(dg.dst));
    }

    /// The frame is a real IPv4 packet, not just something this module can read
    /// back: smoltcp's own parser accepts it, checksum included. If it did not,
    /// every node on the Network would drop what the bridge injected.
    #[test]
    fn a_built_frame_satisfies_smoltcps_parser() {
        let frame = build_udp4(
            Ipv4Address::new(10, 1, 2, 3),
            1234,
            Ipv4Address::new(239, 1, 1, 1),
            5000,
            b"x",
        )
        .unwrap();
        let caps = ChecksumCapabilities::default();
        let pkt = Ipv4Packet::new_checked(&frame[..]).expect("well-formed IPv4");
        let repr = Ipv4Repr::parse(&pkt, &caps).expect("IPv4 header verifies");
        assert_eq!(repr.next_header, IpProtocol::Udp);
        assert!(pkt.verify_checksum(), "IPv4 header checksum");

        let upkt = UdpPacket::new_checked(pkt.payload()).expect("well-formed UDP");
        let urepr = UdpRepr::parse(&upkt, &repr.src_addr.into(), &repr.dst_addr.into(), &caps)
            .expect("UDP header verifies");
        assert_eq!(urepr.dst_port, 5000);
    }

    /// A datagram too big for the fabric to carry is refused rather than
    /// truncated — the caller drops it, as any smaller-MTU hop would.
    #[test]
    fn an_oversized_datagram_is_refused() {
        let big = vec![0u8; 2000];
        assert!(build_udp4(
            Ipv4Address::new(10, 0, 0, 1),
            1,
            Ipv4Address::new(239, 0, 0, 1),
            2,
            &big
        )
        .is_none());
    }

    /// Only UDP over IPv4 is a datagram to bridge. TCP reaches a trunk too and
    /// must not be mistaken for one.
    #[test]
    fn a_tcp_frame_is_not_a_datagram() {
        let ip = Ipv4Repr {
            src_addr: Ipv4Address::new(10, 0, 0, 1),
            dst_addr: Ipv4Address::new(10, 0, 0, 2),
            next_header: IpProtocol::Tcp,
            payload_len: 20,
            hop_limit: 64,
        };
        let mut frame = vec![0u8; ip.buffer_len() + ip.payload_len];
        let mut pkt = Ipv4Packet::new_unchecked(&mut frame[..]);
        ip.emit(&mut pkt, &ChecksumCapabilities::default());
        assert!(parse_udp4(&frame).is_none());
    }

    #[test]
    fn groups_are_the_class_d_range() {
        assert!(is_mcast4(Ipv4Address::new(224, 0, 0, 1)));
        assert!(is_mcast4(Ipv4Address::new(239, 255, 255, 255)));
        assert!(!is_mcast4(Ipv4Address::new(223, 255, 255, 255)));
        assert!(!is_mcast4(Ipv4Address::new(240, 0, 0, 0)));
        assert!(!is_mcast4(Ipv4Address::new(10, 0, 0, 1)));
    }
}
