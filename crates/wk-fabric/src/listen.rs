//! A host-side **TCP listener on the fabric**: the inbound counterpart of
//! [`crate::portfwd`]'s outbound dial. The host joins a node's virtual network
//! as a named peer (fabric DNS resolves the name for same-net nodes) and
//! accepts TCP connections from the fabric; each accepted connection surfaces
//! to the caller as one end of a `UnixStream` socketpair, with a pump thread
//! shuttling bytes between the pair and the smoltcp socket. wk-server uses
//! this to put the wk API on a node's network (`wk-api`'s `serve_client` on
//! the other end of the pair).
//!
//! [`listen`] follows one node, and only that node may connect: an accepted
//! connection whose source address isn't the node's fabric address is dropped
//! (on a shared Network the endpoint would otherwise act with the wired
//! node's authority for whoever dials it — a confused deputy). [`listen_net`]
//! instead joins a network itself and accepts any member — for endpoints that
//! carry no caller authority, like a HostService bridging a host TCP service.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smoltcp::socket::tcp;
use smoltcp::wire::IpAddress;

use crate::netstack::{NetHub, SharedStack, SockKind};

/// Per-direction buffer for each connection's smoltcp socket.
const SOCK_BUF: usize = 64 * 1024;

/// How many sockets sit in Listen at once — the accept backlog. More than one
/// so the port is never momentarily unattended (see the accept loop).
const BACKLOG: usize = 4;

fn tcp_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0u8; SOCK_BUF]),
        tcp::SocketBuffer::new(vec![0u8; SOCK_BUF]),
    )
}

/// Listen on the fabric as a named peer of `follow`'s network. Each accepted
/// connection from `follow` itself is handed to `on_conn` as a `UnixStream`
/// (the pump owns the other end). The listener follows `follow`'s *current*
/// network, so rewiring the node onto another Network moves the endpoint with
/// it. Runs until `kill` is set; then the bridge NIC detaches.
pub fn listen(
    hub: Arc<NetHub>,
    follow: SharedStack,
    name: &str,
    port: u16,
    kill: Arc<AtomicBool>,
    on_conn: Arc<dyn Fn(UnixStream) + Send + Sync>,
) {
    listen_impl(hub, Some(follow), None, name, port, kill, on_conn)
}

/// Listen on the fabric as a named peer of the network `net` itself, accepting
/// connections from *any* member. The scope-wide accept is deliberate — this
/// is for endpoints that carry no caller authority (a HostService bridging a
/// plain TCP service), unlike the Api listener, whose per-node allow-list
/// exists because its connections act with the wired node's token.
pub fn listen_net(
    hub: Arc<NetHub>,
    net: wk_protocol::NodeId,
    name: &str,
    port: u16,
    kill: Arc<AtomicBool>,
    on_conn: Arc<dyn Fn(UnixStream) + Send + Sync>,
) {
    listen_impl(hub, None, Some(net), name, port, kill, on_conn)
}

fn listen_impl(
    hub: Arc<NetHub>,
    follow: Option<SharedStack>,
    fixed_net: Option<wk_protocol::NodeId>,
    name: &str,
    port: u16,
    kill: Arc<AtomicBool>,
    on_conn: Arc<dyn Fn(UnixStream) + Send + Sync>,
) {
    let net = match (&follow, fixed_net) {
        (Some(f), _) => f.lock().unwrap().net,
        (None, Some(n)) => n,
        (None, None) => unreachable!("listen_impl needs a follow stack or a net"),
    };
    let bridge = hub.attach(net, hub.alloc_ip(3), name);
    // Bind before returning, like TcpListener::bind: if the sockets were
    // created inside the accept thread, a caller that resolves the name and
    // connects immediately could land a SYN on an unattended port, and a
    // dropped SYN can fail the connection outright rather than retrying.
    let initial_backlog: Vec<smoltcp::iface::SocketHandle> =
        (0..BACKLOG).map(|_| add_listener(&bridge, port)).collect();

    let accept_thread = std::thread::Builder::new()
        .name(format!("wk-fabric-listen-{name}"))
        .spawn({
            let (bridge, hub) = (bridge.clone(), hub.clone());
            let _ = &hub; // the supervisor below owns the detach
            move || {
                // A real accept backlog. smoltcp has no separate accept(): a
                // listening socket *becomes* the connection, so a single
                // listener leaves a window with nothing listening on the port
                // between taking a connection and adding its replacement — a
                // SYN arriving in that window is refused. Keeping several
                // sockets in Listen closes the window (and lets connections
                // arrive back-to-back, which is what a backlog is for).
                let mut backlog = initial_backlog;
                while !kill.load(Ordering::Relaxed) {
                    // Follow the node's current network (fixed-net listeners
                    // stay where they were attached).
                    if let Some(follow) = &follow {
                        let net = follow.lock().unwrap().net;
                        if bridge.lock().unwrap().net != net {
                            bridge.lock().unwrap().net = net;
                        }
                    }
                    // `None` = any member of the net may connect.
                    let allowed = follow.as_ref().map(|f| {
                        let f = f.lock().unwrap();
                        (IpAddress::from(f.ip), IpAddress::from(f.ip6))
                    });
                    // Collect whatever moved past the handshake this round.
                    let mut accepted: Vec<smoltcp::iface::SocketHandle> = Vec::new();
                    let mut idle = true;
                    {
                        let mut g = bridge.lock().unwrap();
                        backlog.retain(|&h| {
                            let s = g.sockets.get_mut::<tcp::Socket>(h);
                            match s.state() {
                                tcp::State::Listen | tcp::State::SynReceived => true,
                                _ => {
                                    let peer_ok = match allowed {
                                        None => true,
                                        Some((a4, a6)) => s
                                            .remote_endpoint()
                                            .is_some_and(|ep| ep.addr == a4 || ep.addr == a6),
                                    };
                                    if peer_ok {
                                        accepted.push(h);
                                    } else {
                                        // Not the followed node: refuse the deputy.
                                        s.abort();
                                        g.begin_close(h, SockKind::Tcp);
                                    }
                                    idle = false;
                                    false
                                }
                            }
                        });
                    }
                    // Refill first, so the port is never unattended.
                    while backlog.len() < BACKLOG {
                        backlog.push(add_listener(&bridge, port));
                    }
                    for handle in accepted {
                        match UnixStream::pair() {
                            Ok((ours, theirs)) => {
                                let (bridge, kill) = (bridge.clone(), kill.clone());
                                std::thread::spawn(move || pump(ours, bridge, handle, kill));
                                on_conn(theirs);
                            }
                            Err(_) => {
                                let mut g = bridge.lock().unwrap();
                                g.sockets.get_mut::<tcp::Socket>(handle).abort();
                                g.begin_close(handle, SockKind::Tcp);
                            }
                        }
                    }
                    if idle {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                let mut g = bridge.lock().unwrap();
                for h in backlog {
                    g.sockets.get_mut::<tcp::Socket>(h).abort();
                    g.begin_close(h, SockKind::Tcp);
                }
            }
        })
        .expect("spawn fabric listen thread");

    // Detach the bridge NIC once the accept loop is done with it.
    std::thread::Builder::new()
        .name("wk-fabric-listen-detach".into())
        .spawn(move || {
            let _ = accept_thread.join();
            hub.detach(&bridge);
        })
        .expect("spawn fabric listen supervisor");
}

/// Add a fresh listening socket on `port` to the bridge stack.
fn add_listener(bridge: &SharedStack, port: u16) -> smoltcp::iface::SocketHandle {
    let mut g = bridge.lock().unwrap();
    let h = g.sockets.add(tcp_socket());
    let _gen = g.track(h);
    let _ = g.sockets.get_mut::<tcp::Socket>(h).listen(port);
    h
}

/// Shuttle bytes between one end of the socketpair and an accepted smoltcp
/// connection until either side closes (or `kill` is set). The hub thread
/// drives the packet exchange; this thread only moves bytes in and out of the
/// socket buffers — the same discipline as `portfwd::pump`, minus the dial.
fn pump(
    stream: UnixStream,
    bridge: SharedStack,
    handle: smoltcp::iface::SocketHandle,
    kill: Arc<AtomicBool>,
) {
    let mut stream = stream;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(5)));

    let mut to_guest: VecDeque<u8> = VecDeque::new(); // pair -> fabric
    let mut pair_eof = false;
    let mut fin_sent = false;
    let mut tmp = [0u8; 16 * 1024];

    loop {
        if kill.load(Ordering::Relaxed) {
            break;
        }
        if !pair_eof && to_guest.len() < SOCK_BUF {
            match stream.read(&mut tmp) {
                Ok(0) => pair_eof = true,
                Ok(n) => to_guest.extend(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => pair_eof = true,
            }
        }

        let mut to_pair: Vec<u8> = Vec::new();
        let guest_done = {
            let mut g = bridge.lock().unwrap();
            let s = g.sockets.get_mut::<tcp::Socket>(handle);
            while s.can_recv() {
                match s.recv_slice(&mut tmp) {
                    Ok(n) if n > 0 => to_pair.extend_from_slice(&tmp[..n]),
                    _ => break,
                }
            }
            if s.can_send() && !to_guest.is_empty() {
                let chunk = to_guest.make_contiguous();
                if let Ok(n) = s.send_slice(chunk) {
                    to_guest.drain(..n);
                }
            }
            if pair_eof && to_guest.is_empty() && !fin_sent {
                s.close();
                fin_sent = true;
            }
            let state = s.state();
            state == tcp::State::Closed || (!s.may_recv() && !s.can_recv())
        };

        if !to_pair.is_empty() && stream.write_all(&to_pair).is_err() {
            break;
        }
        if guest_done && to_pair.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let mut g = bridge.lock().unwrap();
    if !fin_sent {
        g.sockets.get_mut::<tcp::Socket>(handle).close();
    }
    g.begin_close(handle, SockKind::Tcp);
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::wire::Ipv4Address;
    use wk_protocol::NodeId;

    /// A fabric node dials the named listener and exchanges bytes end-to-end
    /// through the socketpair; a *different* node's connection is refused.
    #[test]
    fn fabric_node_reaches_listener_and_strangers_are_refused() {
        let hub = NetHub::new();
        let net = NodeId::nil();
        let node = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "app");
        let stranger = hub.attach(net, Ipv4Address::new(10, 0, 0, 3), "other");

        let kill = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<UnixStream>();
        listen(
            hub.clone(),
            node.clone(),
            "api",
            1337,
            kill.clone(),
            Arc::new(move |s| {
                let _ = tx.send(s);
            }),
        );

        // Resolve "api" by fabric DNS from the node's net and dial it.
        let api_ip = hub.resolve(net, "api").expect("api resolves on the net");
        let dial = |from: &SharedStack, lp: u16| {
            let mut g = from.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            let _gen = g.track(h);
            let crate::netstack::NodeStack { iface, sockets, .. } = &mut *g;
            sockets
                .get_mut::<tcp::Socket>(h)
                .connect(iface.context(), (IpAddress::from(api_ip), 1337), lp)
                .unwrap();
            h
        };

        let h = dial(&node, 50001);
        // Wait against a wall-clock deadline rather than a fixed iteration
        // count: this runs alongside the rest of the suite, and a loaded
        // machine is slow, not broken.
        let deadline = || std::time::Instant::now() + Duration::from_secs(30);
        let h_deadline = deadline();
        let mut ok = false;
        while std::time::Instant::now() < h_deadline {
            {
                let mut g = node.lock().unwrap();
                let s = g.sockets.get_mut::<tcp::Socket>(h);
                if s.may_send() {
                    ok = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(ok, "node's connection to the api listener establishes");

        // The accepted connection surfaced as a socketpair end.
        let mut server_end = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener handed a connection over");

        // node -> listener
        {
            let mut g = node.lock().unwrap();
            g.sockets
                .get_mut::<tcp::Socket>(h)
                .send_slice(b"hello api\n")
                .unwrap();
        }
        server_end
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 64];
        let mut got = Vec::new();
        while !got.ends_with(b"\n") {
            let n = server_end.read(&mut buf).expect("bytes arrive");
            assert!(n > 0, "eof before the line");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&got, b"hello api\n");

        // listener -> node
        server_end.write_all(b"reply\n").unwrap();
        let mut echoed = Vec::new();
        let echo_deadline = deadline();
        while std::time::Instant::now() < echo_deadline {
            {
                let mut g = node.lock().unwrap();
                let s = g.sockets.get_mut::<tcp::Socket>(h);
                while s.can_recv() {
                    let mut b = [0u8; 64];
                    if let Ok(n) = s.recv_slice(&mut b) {
                        echoed.extend_from_slice(&b[..n]);
                    }
                }
            }
            if echoed.ends_with(b"\n") {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(&echoed, b"reply\n");

        // A stranger on the same net is refused: its connection dies and no
        // socketpair is handed over.
        let hs = dial(&stranger, 50002);
        let stranger_deadline = deadline();
        let mut refused = false;
        while std::time::Instant::now() < stranger_deadline {
            {
                let mut g = stranger.lock().unwrap();
                let s = g.sockets.get_mut::<tcp::Socket>(hs);
                if matches!(s.state(), tcp::State::Closed) && !s.may_recv() {
                    refused = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(refused, "stranger's connection is torn down");
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "no connection surfaced for the stranger"
        );

        kill.store(true, Ordering::Relaxed);
    }

    /// A net-scoped listener accepts any member of the net — both nodes'
    /// connections surface as socketpair ends.
    #[test]
    fn net_listener_accepts_any_member() {
        let hub = NetHub::new();
        let net = NodeId::nil();
        let a = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "a");
        let b = hub.attach(net, Ipv4Address::new(10, 0, 0, 4), "b");

        let kill = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<UnixStream>();
        listen_net(
            hub.clone(),
            net,
            "svc",
            9000,
            kill.clone(),
            Arc::new(move |s| {
                let _ = tx.send(s);
            }),
        );

        let svc_ip = hub.resolve(net, "svc").expect("svc resolves on the net");
        let dial = |from: &SharedStack, lp: u16| {
            let mut g = from.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            let _gen = g.track(h);
            let crate::netstack::NodeStack { iface, sockets, .. } = &mut *g;
            sockets
                .get_mut::<tcp::Socket>(h)
                .connect(iface.context(), (IpAddress::from(svc_ip), 9000), lp)
                .unwrap();
            h
        };

        for (stack, lp) in [(&a, 51001u16), (&b, 51002)] {
            let h = dial(stack, lp);
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut ok = false;
            while std::time::Instant::now() < deadline {
                {
                    let mut g = stack.lock().unwrap();
                    if g.sockets.get_mut::<tcp::Socket>(h).may_send() {
                        ok = true;
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(ok, "member's connection establishes");
            assert!(
                rx.recv_timeout(Duration::from_secs(5)).is_ok(),
                "connection surfaced for the member"
            );
        }

        kill.store(true, Ordering::Relaxed);
    }
}
