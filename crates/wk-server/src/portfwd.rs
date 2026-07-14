//! Publishing a fabric node's TCP service on a localhost port: the inbound
//! counterpart of the gateway bridge in [`crate::sockets`]. When a HostPort is
//! wired to a node that serves over `wasi:sockets` (rather than exporting
//! `wasi:http`), the host binds `127.0.0.1:port` and joins the node's virtual
//! network as a peer of its own — each accepted host connection dials the node
//! over the fabric at the *same* port number, and a pump thread shuttles bytes
//! between the host socket and the smoltcp connection. Because the bridge is a
//! real fabric peer, its traffic is ordinary IP packets on the node's network:
//! middlebox nodes in the path see it like any node-to-node flow.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smoltcp::socket::tcp;
use wasmtime::Result;

use crate::netstack::{NetHub, SharedStack, SockKind};

/// Per-direction buffer for each forwarded connection's smoltcp socket.
const SOCK_BUF: usize = 64 * 1024;

fn tcp_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0u8; SOCK_BUF]),
        tcp::SocketBuffer::new(vec![0u8; SOCK_BUF]),
    )
}

/// Forward `127.0.0.1:port` to `target`'s fabric address at the same port.
/// Binds synchronously (so a bind failure is reported to the caller), then
/// accepts on a background thread until `kill` is set. The bridge NIC follows
/// the target's *current* network, so rewiring the node onto another Network
/// applies to new connections without restarting the forward.
pub fn forward(
    hub: Arc<NetHub>,
    target: SharedStack,
    port: u16,
    kill: Arc<AtomicBool>,
) -> Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener =
        TcpListener::bind(addr).map_err(|e| wasmtime::Error::msg(format!("bind {addr}: {e}")))?;
    // Nonblocking so the accept loop can poll the kill flag.
    listener.set_nonblocking(true)?;

    let net = target.lock().unwrap().net;
    // The bridge gets its own address so replies route back to it, not to a
    // node. Unnamed, so it never shadows a node in fabric DNS.
    let bridge = hub.attach(net, hub.alloc_ip(2), "");

    std::thread::Builder::new()
        .name(format!("wk-portfwd-{port}"))
        .spawn(move || {
            // Ephemeral local port per outgoing fabric connection.
            let mut local_port: u16 = 49152;
            while !kill.load(Ordering::Relaxed) {
                // Track the target's current network (rewiring takes effect here).
                let net = target.lock().unwrap().net;
                if bridge.lock().unwrap().net != net {
                    bridge.lock().unwrap().net = net;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let dst = target.lock().unwrap().ip;
                        let (bridge, kill) = (bridge.clone(), kill.clone());
                        local_port = local_port.checked_add(1).unwrap_or(49152);
                        let lp = local_port;
                        std::thread::spawn(move || {
                            pump(stream, bridge, dst.into(), port, lp, kill)
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
            hub.detach(&bridge);
        })
        .expect("spawn portfwd thread");
    Ok(())
}

/// Shuttle bytes between one accepted host connection and a fresh smoltcp
/// connection to `dst:dst_port` on the bridge stack, until either side closes
/// (or `kill` is set). The hub thread drives the actual packet exchange; this
/// thread only moves bytes in and out of the socket buffers.
fn pump(
    stream: TcpStream,
    bridge: SharedStack,
    dst: smoltcp::wire::IpAddress,
    dst_port: u16,
    local_port: u16,
    kill: Arc<AtomicBool>,
) {
    let mut stream = stream;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(5)));
    let _ = stream.set_nodelay(true);

    let handle = {
        let mut g = bridge.lock().unwrap();
        let h = g.sockets.add(tcp_socket());
        // Track + begin_close below so the hub reaps the socket once drained.
        let _gen = g.track(h);
        let crate::netstack::NodeStack { iface, sockets, .. } = &mut *g;
        if sockets
            .get_mut::<tcp::Socket>(h)
            .connect(iface.context(), (dst, dst_port), local_port)
            .is_err()
        {
            g.begin_close(h, SockKind::Tcp);
            return;
        }
        h
    };

    let mut to_guest: VecDeque<u8> = VecDeque::new(); // host -> fabric
    let mut host_eof = false;
    let mut fin_sent = false;
    let mut saw_open = false; // connection reached Established at some point
    let mut tmp = [0u8; 16 * 1024];

    loop {
        if kill.load(Ordering::Relaxed) {
            break;
        }
        // Host -> buffer (bounded, so a stalled guest applies backpressure).
        if !host_eof && to_guest.len() < SOCK_BUF {
            match stream.read(&mut tmp) {
                Ok(0) => host_eof = true,
                Ok(n) => to_guest.extend(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => host_eof = true,
            }
        }

        // Exchange with the smoltcp socket under the stack lock.
        let mut to_host: Vec<u8> = Vec::new();
        let guest_done = {
            let mut g = bridge.lock().unwrap();
            let s = g.sockets.get_mut::<tcp::Socket>(handle);
            if s.may_send() || s.may_recv() {
                saw_open = true;
            }
            while s.can_recv() {
                match s.recv_slice(&mut tmp) {
                    Ok(n) if n > 0 => to_host.extend_from_slice(&tmp[..n]),
                    _ => break,
                }
            }
            if s.can_send() && !to_guest.is_empty() {
                let chunk = to_guest.make_contiguous();
                if let Ok(n) = s.send_slice(chunk) {
                    to_guest.drain(..n);
                }
            }
            if host_eof && to_guest.is_empty() && !fin_sent {
                s.close();
                fin_sent = true;
            }
            let state = s.state();
            // Guest side finished: fully closed, or refused (never opened), or
            // it stopped sending and everything received is drained.
            state == tcp::State::Closed || (saw_open && !s.may_recv() && !s.can_recv())
        };

        if !to_host.is_empty() && stream.write_all(&to_host).is_err() {
            break;
        }
        if guest_done && to_host.is_empty() {
            break;
        }
        if host_eof && fin_sent && guest_done {
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

    /// A localhost port claimed and immediately released, for the forwarder to
    /// bind. Racy in principle, standard in practice.
    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A real host TCP client reaches a listener on the fabric through the
    /// forward: connect to 127.0.0.1, get echoed by the smoltcp-side "guest".
    #[test]
    fn host_connection_reaches_a_fabric_listener() {
        let hub = NetHub::new();
        let net = NodeId::nil();
        let server = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "server");
        let port = free_port();

        // The "guest": a raw smoltcp listener on the same port, echoing.
        let server_h = {
            let mut g = server.lock().unwrap();
            let h = g.sockets.add(tcp_socket());
            g.sockets.get_mut::<tcp::Socket>(h).listen(port).unwrap();
            h
        };
        let echo_stack = server.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            for _ in 0..5000 {
                {
                    let mut g = echo_stack.lock().unwrap();
                    let s = g.sockets.get_mut::<tcp::Socket>(server_h);
                    if s.can_recv() {
                        if let Ok(n) = s.recv_slice(&mut buf) {
                            if n > 0 && s.can_send() {
                                s.send_slice(&buf[..n]).unwrap();
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        let kill = Arc::new(AtomicBool::new(false));
        forward(hub.clone(), server.clone(), port, kill.clone()).unwrap();

        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(b"ping over the fabric").unwrap();

        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < 20 {
            match c.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) => panic!("read from forwarded connection: {e}"),
            }
        }
        assert_eq!(&got, b"ping over the fabric");

        kill.store(true, Ordering::Relaxed);
    }

    /// Killing the forward releases the localhost port so it can be rebound
    /// (the reconcile loop depends on this to move a serve between nodes).
    #[test]
    fn kill_releases_the_port() {
        let hub = NetHub::new();
        let server = hub.attach(NodeId::nil(), Ipv4Address::new(10, 0, 0, 2), "server");
        let port = free_port();
        let kill = Arc::new(AtomicBool::new(false));
        forward(hub, server, port, kill.clone()).unwrap();
        kill.store(true, Ordering::Relaxed);
        // The accept loop notices the kill within one poll interval.
        let mut rebound = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(20));
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                rebound = true;
                break;
            }
        }
        assert!(rebound, "port {port} still bound after kill");
    }
}
