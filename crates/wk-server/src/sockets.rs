//! wk's own `wasi:sockets` host implementation, backed by per-node smoltcp
//! stacks on the [`crate::netstack`] fabric (not the host OS). A recompiled C
//! program's BSD socket calls (via wasi-libc → wasi:sockets) drive a smoltcp
//! socket on the node's stack; the hub thread routes its packets to peers on the
//! same virtual network. A node with no network attached can't reach anything.
//!
//! TCP (connect/bind/listen/accept + streams) and UDP (bind/stream/send/receive
//! datagrams) are both implemented over the node's smoltcp stack, dual-stack
//! (IPv4 `10.0.0.0/24` + IPv6 ULA `fd00::/64`). A node wired to a Gateway node
//! gets host access: off-fabric connections are bridged to real host sockets and
//! names resolve via the host resolver; otherwise only fabric peers and numeric
//! addresses are reachable. Sockets are reaped (removed from the node's set) once
//! their wasi resource drops and the connection drains, via the hub.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::Ipv4Address;
use wasmtime::component::{HasData, Linker, Resource};
use wasmtime::Result;
use wasmtime_wasi::p2::{subscribe, DynPollable, Pollable};
use wasmtime_wasi_io::async_trait;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::streams::{
    DynInputStream, DynOutputStream, InputStream, OutputStream, StreamError, StreamResult,
};
use wasmtime_wasi_io::IoView;

use crate::netstack::{NodeStack, SharedStack};
use crate::plugin::HostState;

wasmtime::component::bindgen!({
    path: "wit-sockets",
    world: "sockets-host",
    imports: { default: trappable },
    require_store_data_send: true,
    with: {
        "wasi:io/error": wasmtime_wasi_io::bindings::wasi::io::error,
        "wasi:io/poll": wasmtime_wasi_io::bindings::wasi::io::poll,
        "wasi:io/streams": wasmtime_wasi_io::bindings::wasi::io::streams,
        "wasi:sockets/network.network": Net,
        "wasi:sockets/tcp.tcp-socket": TcpSock,
        "wasi:sockets/udp.udp-socket": UdpSock,
        "wasi:sockets/udp.incoming-datagram-stream": Datagrams,
        "wasi:sockets/udp.outgoing-datagram-stream": Datagrams,
        "wasi:sockets/ip-name-lookup.resolve-address-stream": ResolveStream,
    },
});

use wasi::sockets::network::{
    ErrorCode, IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress,
};
use wasi::sockets::tcp::ShutdownType;

const TCP_BUF: usize = 64 * 1024;
const UDP_BUF: usize = 64 * 1024;
/// Number of datagram slots in a UDP socket's packet ring buffers.
const UDP_SLOTS: usize = 64;

/// Per-node socket context held in `HostState`: the node's smoltcp stack, its
/// address, and the next ephemeral local port to hand out.
pub struct NetCtx {
    pub stack: SharedStack,
    pub next_port: u16,
    /// The fabric hub, for resolving peer node names on this node's network.
    pub hub: std::sync::Arc<crate::netstack::NetHub>,
}

impl NetCtx {
    pub fn new(stack: SharedStack, hub: std::sync::Arc<crate::netstack::NetHub>) -> Self {
        NetCtx {
            stack,
            next_port: 49152,
            hub,
        }
    }
}

// ---- resources ----

/// The `network` resource — an opaque capability handle; the actual stack lives
/// in `HostState`.
pub struct Net;

/// A TCP socket: a smoltcp socket handle in the node's set, plus the bits wasi's
/// start/finish dance needs.
pub struct TcpSock {
    handle: SocketHandle,
    family: IpAddressFamily,
    /// Port set by `start-bind`, used by `start-listen`.
    bound_port: u16,
    local: Option<IpSocketAddress>,
    remote: Option<IpSocketAddress>,
    listening: bool,
    /// Set when the socket connects off-fabric through a host gateway; its bytes
    /// flow over a real host socket instead of smoltcp.
    host: Option<HostConn>,
}

// ---- host gateway: bridge an off-fabric connection to a real host socket ----

#[derive(Default)]
struct Pipe {
    buf: std::collections::VecDeque<u8>,
    closed: bool,
    wakers: Vec<std::task::Waker>,
}
type SharedPipe = std::sync::Arc<std::sync::Mutex<Pipe>>;

fn wake_pipe(p: &SharedPipe) {
    for w in p.lock().unwrap().wakers.drain(..) {
        w.wake();
    }
}

/// A connection to the real host network, bridged to the guest's byte streams by
/// a per-connection thread. Created only for nodes wired to a Gateway node.
struct HostConn {
    incoming: SharedPipe, // host -> guest
    outgoing: SharedPipe, // guest -> host
    connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HostConn {
    fn connect(ip: smoltcp::wire::IpAddress, port: u16) -> HostConn {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let incoming: SharedPipe = Default::default();
        let outgoing: SharedPipe = Default::default();
        let connected = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let (inc, out, conn, fail) = (
            incoming.clone(),
            outgoing.clone(),
            connected.clone(),
            failed.clone(),
        );
        let addr = match ip {
            smoltcp::wire::IpAddress::Ipv4(v4) => std::net::SocketAddr::from((v4.octets(), port)),
            smoltcp::wire::IpAddress::Ipv6(v6) => std::net::SocketAddr::from((v6.octets(), port)),
        };
        std::thread::spawn(move || {
            match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(10)) {
                Ok(stream) => {
                    conn.store(true, std::sync::atomic::Ordering::Relaxed);
                    wake_pipe(&inc);
                    host_pump(stream, inc, out);
                }
                Err(_) => {
                    fail.store(true, std::sync::atomic::Ordering::Relaxed);
                    wake_pipe(&inc);
                }
            }
        });
        HostConn {
            incoming,
            outgoing,
            connected,
            failed,
        }
    }
}

/// Pump bytes between a real host socket and the guest pipes until either side
/// closes. A short read timeout lets one thread service both directions.
fn host_pump(mut stream: std::net::TcpStream, incoming: SharedPipe, outgoing: SharedPipe) {
    use std::io::{Read, Write};
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(10)));
    let mut tmp = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => {
                incoming.lock().unwrap().closed = true;
                wake_pipe(&incoming);
                break;
            }
            Ok(n) => {
                incoming.lock().unwrap().buf.extend(&tmp[..n]);
                wake_pipe(&incoming);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => {
                incoming.lock().unwrap().closed = true;
                wake_pipe(&incoming);
                break;
            }
        }
        let (out, guest_closed) = {
            let mut g = outgoing.lock().unwrap();
            (g.buf.drain(..).collect::<Vec<u8>>(), g.closed)
        };
        if !out.is_empty() && stream.write_all(&out).is_err() {
            incoming.lock().unwrap().closed = true;
            wake_pipe(&incoming);
            break;
        }
        if guest_closed {
            let _ = stream.shutdown(std::net::Shutdown::Write);
        }
    }
}

/// A UDP socket: a smoltcp udp socket handle in the node's set, plus the bits
/// wasi's bind/stream dance needs.
pub struct UdpSock {
    handle: SocketHandle,
    family: IpAddressFamily,
    /// Port set by `start-bind` / chosen at `finish-bind`.
    bound_port: u16,
    local: Option<IpSocketAddress>,
    /// Default peer set by the most recent `stream` call (POSIX "connected").
    remote: Option<IpSocketAddress>,
    bound: bool,
}

/// An incoming or outgoing datagram stream over a node's UDP socket. For an
/// outgoing stream `remote` is the default destination; for an incoming stream
/// it filters received datagrams to that peer (per the wasi `stream` contract).
pub struct Datagrams {
    stack: SharedStack,
    handle: SocketHandle,
    remote: Option<IpSocketAddress>,
}

/// A name-resolution result stream (numeric literals only for now).
pub struct ResolveStream {
    addrs: std::collections::VecDeque<IpAddress>,
}

// ---- helpers ----

fn ipv4(addr: Ipv4Address) -> (u8, u8, u8, u8) {
    let o = addr.octets();
    (o[0], o[1], o[2], o[3])
}

fn to_ipv4(a: (u8, u8, u8, u8)) -> Ipv4Address {
    Ipv4Address::new(a.0, a.1, a.2, a.3)
}

/// A wasi socket address (v4 or v6) -> a smoltcp address + port.
fn to_smol(a: IpSocketAddress) -> (smoltcp::wire::IpAddress, u16) {
    match a {
        IpSocketAddress::Ipv4(s) => (to_ipv4(s.address).into(), s.port),
        IpSocketAddress::Ipv6(s) => {
            let g = s.address;
            let v6 = std::net::Ipv6Addr::new(g.0, g.1, g.2, g.3, g.4, g.5, g.6, g.7);
            (v6.into(), s.port)
        }
    }
}

/// A smoltcp address + port -> a wasi socket address.
fn from_smol(ip: smoltcp::wire::IpAddress, port: u16) -> IpSocketAddress {
    match ip {
        smoltcp::wire::IpAddress::Ipv4(v4) => IpSocketAddress::Ipv4(Ipv4SocketAddress {
            port,
            address: ipv4(v4),
        }),
        smoltcp::wire::IpAddress::Ipv6(v6) => {
            let s = v6.segments();
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port,
                flow_info: 0,
                address: (s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]),
                scope_id: 0,
            })
        }
    }
}

/// This node's own fabric address in the family of `remote` (v4 or v6).
fn local_ip_for(stack: &SharedStack, remote: smoltcp::wire::IpAddress) -> smoltcp::wire::IpAddress {
    let g = stack.lock().unwrap();
    match remote {
        smoltcp::wire::IpAddress::Ipv4(_) => g.ip.into(),
        smoltcp::wire::IpAddress::Ipv6(_) => g.ip6.into(),
    }
}

impl HostState {
    /// This node's stack handle, or `None` if it has no network attached.
    fn stack(&self) -> Option<SharedStack> {
        self.net.as_ref().map(|n| n.stack.clone())
    }
}

// ---- linker ----

struct HasSock;
impl HasData for HasSock {
    type Data<'a> = &'a mut HostState;
}

pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    let net_opts = wasi::sockets::network::LinkOptions::default();
    wasi::sockets::network::add_to_linker::<_, HasSock>(l, &net_opts, |s| s)?;
    wasi::sockets::instance_network::add_to_linker::<_, HasSock>(l, |s| s)?;
    wasi::sockets::ip_name_lookup::add_to_linker::<_, HasSock>(l, |s| s)?;
    wasi::sockets::tcp::add_to_linker::<_, HasSock>(l, |s| s)?;
    wasi::sockets::tcp_create_socket::add_to_linker::<_, HasSock>(l, |s| s)?;
    wasi::sockets::udp::add_to_linker::<_, HasSock>(l, |s| s)?;
    wasi::sockets::udp_create_socket::add_to_linker::<_, HasSock>(l, |s| s)?;
    Ok(())
}

// ---- pollables ----

/// What a socket pollable is waiting for.
#[derive(Clone, Copy)]
enum Want {
    /// A connect/listen/accept handshake to settle (not mid-handshake).
    Event(SocketHandle),
    /// Readable: data available or peer closed.
    Read(SocketHandle),
    /// Writable: send buffer has room (or socket closed).
    Write(SocketHandle),
}

struct SockPollable {
    stack: SharedStack,
    want: Want,
}

#[async_trait]
impl Pollable for SockPollable {
    async fn ready(&mut self) {
        WantReady {
            stack: self.stack.clone(),
            want: self.want,
        }
        .await
    }
}

struct WantReady {
    stack: SharedStack,
    want: Want,
}

impl Future for WantReady {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let mut g = self.stack.lock().unwrap();
        let handle = match self.want {
            Want::Event(h) | Want::Read(h) | Want::Write(h) => h,
        };
        // The socket was reaped (its owner dropped): resolve so the awaiter
        // unblocks and the following operation sees the closed socket.
        if !g.is_live(handle) {
            return Poll::Ready(());
        }
        let ready = {
            let s = g.sockets.get::<tcp::Socket>(handle);
            match self.want {
                Want::Event(_) => !matches!(
                    s.state(),
                    tcp::State::Listen | tcp::State::SynSent | tcp::State::SynReceived
                ),
                Want::Read(_) => s.can_recv() || !s.may_recv(),
                Want::Write(_) => s.can_send() || !s.may_send(),
            }
        };
        if ready {
            Poll::Ready(())
        } else {
            g.park(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Ready when a UDP socket can send (`send`) or has a datagram queued (`!send`).
struct UdpReady {
    stack: SharedStack,
    handle: SocketHandle,
    send: bool,
}
impl Future for UdpReady {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let mut g = self.stack.lock().unwrap();
        if !g.is_live(self.handle) {
            return Poll::Ready(());
        }
        let s = g.sockets.get::<udp::Socket>(self.handle);
        let ready = if self.send {
            s.can_send()
        } else {
            s.can_recv()
        };
        if ready {
            Poll::Ready(())
        } else {
            g.park(cx.waker().clone());
            Poll::Pending
        }
    }
}

struct UdpStreamPollable {
    stack: SharedStack,
    handle: SocketHandle,
    send: bool,
}
#[async_trait]
impl Pollable for UdpStreamPollable {
    async fn ready(&mut self) {
        UdpReady {
            stack: self.stack.clone(),
            handle: self.handle,
            send: self.send,
        }
        .await
    }
}

/// A pollable that's always ready (used for the numeric DNS stream).
struct ReadyNow;
#[async_trait]
impl Pollable for ReadyNow {
    async fn ready(&mut self) {}
}

// ---- TCP byte streams over a smoltcp socket ----

struct TcpInput {
    stack: SharedStack,
    handle: SocketHandle,
}

#[async_trait]
impl Pollable for TcpInput {
    async fn ready(&mut self) {
        WantReady {
            stack: self.stack.clone(),
            want: Want::Read(self.handle),
        }
        .await
    }
}

impl InputStream for TcpInput {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        let mut g = self.stack.lock().unwrap();
        if !g.is_live(self.handle) {
            return Err(StreamError::Closed);
        }
        let s = g.sockets.get_mut::<tcp::Socket>(self.handle);
        if s.can_recv() {
            let mut buf = vec![0u8; size.min(TCP_BUF)];
            let n = s.recv_slice(&mut buf).map_err(|_| StreamError::Closed)?;
            buf.truncate(n);
            Ok(Bytes::from(buf))
        } else if s.may_recv() {
            Ok(Bytes::new()) // open, nothing yet
        } else {
            Err(StreamError::Closed) // peer closed, drained
        }
    }
}

struct TcpOutput {
    stack: SharedStack,
    handle: SocketHandle,
}

#[async_trait]
impl Pollable for TcpOutput {
    async fn ready(&mut self) {
        WantReady {
            stack: self.stack.clone(),
            want: Want::Write(self.handle),
        }
        .await
    }
}

impl OutputStream for TcpOutput {
    fn check_write(&mut self) -> StreamResult<usize> {
        let mut g = self.stack.lock().unwrap();
        if !g.is_live(self.handle) {
            return Err(StreamError::Closed);
        }
        let s = g.sockets.get_mut::<tcp::Socket>(self.handle);
        if !s.may_send() {
            return Err(StreamError::Closed);
        }
        Ok(s.send_capacity().saturating_sub(s.send_queue()))
    }
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut g = self.stack.lock().unwrap();
        if !g.is_live(self.handle) {
            return Err(StreamError::Closed);
        }
        let s = g.sockets.get_mut::<tcp::Socket>(self.handle);
        s.send_slice(&bytes).map_err(|_| StreamError::Closed)?;
        Ok(())
    }
    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }
}

// ---- host gateway byte streams + pollables ----

/// Ready when a pipe has data or is closed.
struct PipeReady {
    pipe: SharedPipe,
}
impl Future for PipeReady {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let mut p = self.pipe.lock().unwrap();
        if !p.buf.is_empty() || p.closed {
            Poll::Ready(())
        } else {
            p.wakers.push(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Ready when a host connect attempt has settled (connected or failed).
struct ConnReady {
    pipe: SharedPipe,
    connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Future for ConnReady {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        use std::sync::atomic::Ordering;
        if self.connected.load(Ordering::Relaxed) || self.failed.load(Ordering::Relaxed) {
            Poll::Ready(())
        } else {
            self.pipe.lock().unwrap().wakers.push(cx.waker().clone());
            Poll::Pending
        }
    }
}

struct HostInput {
    pipe: SharedPipe,
}
#[async_trait]
impl Pollable for HostInput {
    async fn ready(&mut self) {
        PipeReady {
            pipe: self.pipe.clone(),
        }
        .await
    }
}
impl InputStream for HostInput {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        let mut p = self.pipe.lock().unwrap();
        if p.buf.is_empty() {
            return if p.closed {
                Err(StreamError::Closed)
            } else {
                Ok(Bytes::new())
            };
        }
        let n = size.min(p.buf.len());
        let bytes: Vec<u8> = p.buf.drain(..n).collect();
        Ok(Bytes::from(bytes))
    }
}

struct HostOutput {
    pipe: SharedPipe,
}
#[async_trait]
impl Pollable for HostOutput {
    async fn ready(&mut self) {} // always writable (buffered)
}
impl OutputStream for HostOutput {
    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(64 * 1024)
    }
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.pipe.lock().unwrap().buf.extend(bytes.iter());
        Ok(())
    }
    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }
}

/// A pollable for a host-gateway socket: ready once the connect settles.
struct HostEventPollable {
    pipe: SharedPipe,
    connected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait]
impl Pollable for HostEventPollable {
    async fn ready(&mut self) {
        ConnReady {
            pipe: self.pipe.clone(),
            connected: self.connected.clone(),
            failed: self.failed.clone(),
        }
        .await
    }
}

/// Is `ip` on the virtual fabric (IPv4 `10.0.0.0/24` or IPv6 ULA `fd00::/8`)
/// rather than the host network?
fn on_fabric(ip: smoltcp::wire::IpAddress) -> bool {
    match ip {
        smoltcp::wire::IpAddress::Ipv4(v4) => {
            let o = v4.octets();
            o[0] == 10 && o[1] == 0 && o[2] == 0
        }
        smoltcp::wire::IpAddress::Ipv6(v6) => v6.octets()[0] == 0xfd,
    }
}

// ---- interface impls ----

impl wasi::sockets::network::Host for HostState {
    fn network_error_code(
        &mut self,
        _err: Resource<wasmtime_wasi_io::streams::Error>,
    ) -> Result<Option<ErrorCode>> {
        Ok(None)
    }
}
impl wasi::sockets::network::HostNetwork for HostState {
    fn drop(&mut self, rep: Resource<Net>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

impl wasi::sockets::instance_network::Host for HostState {
    fn instance_network(&mut self) -> Result<Resource<Net>> {
        Ok(self.table().push(Net)?)
    }
}

impl wasi::sockets::tcp_create_socket::Host for HostState {
    fn create_tcp_socket(
        &mut self,
        family: IpAddressFamily,
    ) -> Result<std::result::Result<Resource<TcpSock>, ErrorCode>> {
        let Some(stack) = self.stack() else {
            return Ok(Err(ErrorCode::AccessDenied));
        };
        let handle = {
            let sock = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0u8; TCP_BUF]),
                tcp::SocketBuffer::new(vec![0u8; TCP_BUF]),
            );
            let mut g = stack.lock().unwrap();
            let h = g.sockets.add(sock);
            g.track(h);
            h
        };
        Ok(Ok(self.table().push(TcpSock {
            handle,
            family,
            bound_port: 0,
            local: None,
            remote: None,
            listening: false,
            host: None,
        })?))
    }
}

impl wasi::sockets::tcp::Host for HostState {}

impl wasi::sockets::tcp::HostTcpSocket for HostState {
    fn start_bind(
        &mut self,
        this: Resource<TcpSock>,
        _network: Resource<Net>,
        local_address: IpSocketAddress,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (_, port) = to_smol(local_address);
        let s = self.table().get_mut(&this)?;
        s.bound_port = port;
        s.local = Some(local_address);
        Ok(Ok(()))
    }
    fn finish_bind(
        &mut self,
        _this: Resource<TcpSock>,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }

    fn start_connect(
        &mut self,
        this: Resource<TcpSock>,
        _network: Resource<Net>,
        remote_address: IpSocketAddress,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let Some(stack) = self.stack() else {
            return Ok(Err(ErrorCode::AccessDenied));
        };
        let (remote_ip, remote_port) = to_smol(remote_address);
        // Off-fabric destination: bridge to the real host network, but only if
        // this node is wired to a Gateway (host access granted).
        if !on_fabric(remote_ip) {
            if !stack.lock().unwrap().host_access {
                return Ok(Err(ErrorCode::AccessDenied));
            }
            let conn = HostConn::connect(remote_ip, remote_port);
            let s = self.table().get_mut(&this)?;
            s.host = Some(conn);
            s.remote = Some(remote_address);
            return Ok(Ok(()));
        }
        let local_ip = local_ip_for(&stack, remote_ip);
        let lport = {
            let ctx = self.net.as_mut().unwrap();
            let p = ctx.next_port;
            ctx.next_port = ctx.next_port.checked_add(1).unwrap_or(49152);
            p
        };
        let handle = self.table().get(&this)?.handle;
        {
            let mut g = stack.lock().unwrap();
            let NodeStack { iface, sockets, .. } = &mut *g;
            let s = sockets.get_mut::<tcp::Socket>(handle);
            if s.connect(iface.context(), (remote_ip, remote_port), lport)
                .is_err()
            {
                return Ok(Err(ErrorCode::InvalidState));
            }
        }
        let s = self.table().get_mut(&this)?;
        s.remote = Some(remote_address);
        s.local = Some(from_smol(local_ip, lport));
        Ok(Ok(()))
    }
    fn finish_connect(
        &mut self,
        this: Resource<TcpSock>,
    ) -> Result<std::result::Result<(Resource<DynInputStream>, Resource<DynOutputStream>), ErrorCode>>
    {
        // Host-gateway connection?
        let host = {
            let s = self.table().get(&this)?;
            s.host.as_ref().map(|h| {
                (
                    h.incoming.clone(),
                    h.outgoing.clone(),
                    h.connected.clone(),
                    h.failed.clone(),
                )
            })
        };
        if let Some((inc, out, connected, failed)) = host {
            use std::sync::atomic::Ordering;
            if failed.load(Ordering::Relaxed) {
                return Ok(Err(ErrorCode::ConnectionRefused));
            }
            if !connected.load(Ordering::Relaxed) {
                return Ok(Err(ErrorCode::WouldBlock));
            }
            let input: DynInputStream = Box::new(HostInput { pipe: inc });
            let output: DynOutputStream = Box::new(HostOutput { pipe: out });
            let i = self.table().push(input)?;
            let o = self.table().push(output)?;
            return Ok(Ok((i, o)));
        }
        let Some(stack) = self.stack() else {
            return Ok(Err(ErrorCode::AccessDenied));
        };
        let handle = self.table().get(&this)?.handle;
        let state = stack
            .lock()
            .unwrap()
            .sockets
            .get::<tcp::Socket>(handle)
            .state();
        match state {
            tcp::State::Established => {
                let input: DynInputStream = Box::new(TcpInput {
                    stack: stack.clone(),
                    handle,
                });
                let output: DynOutputStream = Box::new(TcpOutput { stack, handle });
                let i = self.table().push(input)?;
                let o = self.table().push(output)?;
                Ok(Ok((i, o)))
            }
            tcp::State::SynSent | tcp::State::SynReceived => Ok(Err(ErrorCode::WouldBlock)),
            _ => Ok(Err(ErrorCode::ConnectionRefused)),
        }
    }

    fn start_listen(
        &mut self,
        this: Resource<TcpSock>,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let Some(stack) = self.stack() else {
            return Ok(Err(ErrorCode::AccessDenied));
        };
        let (handle, port) = {
            let s = self.table().get(&this)?;
            (s.handle, s.bound_port)
        };
        {
            let mut g = stack.lock().unwrap();
            if g.sockets
                .get_mut::<tcp::Socket>(handle)
                .listen(port)
                .is_err()
            {
                return Ok(Err(ErrorCode::InvalidState));
            }
        }
        self.table().get_mut(&this)?.listening = true;
        Ok(Ok(()))
    }
    fn finish_listen(
        &mut self,
        _this: Resource<TcpSock>,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }

    fn accept(
        &mut self,
        this: Resource<TcpSock>,
    ) -> Result<
        std::result::Result<
            (
                Resource<TcpSock>,
                Resource<DynInputStream>,
                Resource<DynOutputStream>,
            ),
            ErrorCode,
        >,
    > {
        let Some(stack) = self.stack() else {
            return Ok(Err(ErrorCode::AccessDenied));
        };
        let (listen_handle, family, port) = {
            let s = self.table().get(&this)?;
            (s.handle, s.family, s.bound_port)
        };
        // A peer has connected once the listening socket reaches Established.
        let conn_handle = {
            let mut g = stack.lock().unwrap();
            let st = g.sockets.get::<tcp::Socket>(listen_handle).state();
            if st != tcp::State::Established {
                return Ok(Err(ErrorCode::WouldBlock));
            }
            // Keep accepting: add a fresh listening socket on the same port and
            // hand the established one out as the accepted connection.
            let fresh = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0u8; TCP_BUF]),
                tcp::SocketBuffer::new(vec![0u8; TCP_BUF]),
            );
            let new_listen = g.sockets.add(fresh);
            g.track(new_listen);
            if g.sockets
                .get_mut::<tcp::Socket>(new_listen)
                .listen(port)
                .is_err()
            {
                return Ok(Err(ErrorCode::Unknown));
            }
            // Point the listener resource at the new socket.
            self.table().get_mut(&this)?.handle = new_listen;
            listen_handle
        };
        let conn = self.table().push(TcpSock {
            handle: conn_handle,
            family,
            bound_port: port,
            local: None,
            remote: None,
            listening: false,
            host: None,
        })?;
        let input: DynInputStream = Box::new(TcpInput {
            stack: stack.clone(),
            handle: conn_handle,
        });
        let output: DynOutputStream = Box::new(TcpOutput {
            stack,
            handle: conn_handle,
        });
        let i = self.table().push(input)?;
        let o = self.table().push(output)?;
        Ok(Ok((conn, i, o)))
    }

    fn local_address(
        &mut self,
        this: Resource<TcpSock>,
    ) -> Result<std::result::Result<IpSocketAddress, ErrorCode>> {
        match self.table().get(&this)?.local {
            Some(a) => Ok(Ok(a)),
            None => Ok(Err(ErrorCode::InvalidState)),
        }
    }
    fn remote_address(
        &mut self,
        this: Resource<TcpSock>,
    ) -> Result<std::result::Result<IpSocketAddress, ErrorCode>> {
        match self.table().get(&this)?.remote {
            Some(a) => Ok(Ok(a)),
            None => Ok(Err(ErrorCode::InvalidState)),
        }
    }
    fn is_listening(&mut self, this: Resource<TcpSock>) -> Result<bool> {
        Ok(self.table().get(&this)?.listening)
    }
    fn address_family(&mut self, this: Resource<TcpSock>) -> Result<IpAddressFamily> {
        Ok(self.table().get(&this)?.family)
    }

    fn subscribe(&mut self, this: Resource<TcpSock>) -> Result<Resource<DynPollable>> {
        // Host-gateway socket: readiness tracks the host connect.
        let host = {
            let s = self.table().get(&this)?;
            s.host
                .as_ref()
                .map(|h| (h.incoming.clone(), h.connected.clone(), h.failed.clone()))
        };
        if let Some((pipe, connected, failed)) = host {
            let p = self.table().push(HostEventPollable {
                pipe,
                connected,
                failed,
            })?;
            return subscribe(self.table(), p);
        }
        let stack = self.stack().expect("socket exists => has a stack");
        let handle = self.table().get(&this)?.handle;
        let p = self.table().push(SockPollable {
            stack,
            want: Want::Event(handle),
        })?;
        subscribe(self.table(), p)
    }

    fn shutdown(
        &mut self,
        this: Resource<TcpSock>,
        _how: ShutdownType,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        if let Some(stack) = self.stack() {
            let handle = self.table().get(&this)?.handle;
            stack
                .lock()
                .unwrap()
                .sockets
                .get_mut::<tcp::Socket>(handle)
                .close();
        }
        Ok(Ok(()))
    }

    fn drop(&mut self, rep: Resource<TcpSock>) -> Result<()> {
        // Reap the smoltcp socket: close it (best-effort FIN) and remove it from
        // the set so its buffers are freed — a long-lived node opening many
        // connections would otherwise leak a socket per connection. The guest is
        // done with it (its byte streams do no I/O after the socket is dropped).
        if let Some(stack) = self.stack() {
            let handle = self.table().get(&rep)?.handle;
            let mut g = stack.lock().unwrap();
            // Queue a graceful FIN, then hand the socket to the hub to reap once
            // its pending data and FIN have flushed.
            if g.is_live(handle) {
                g.sockets.get_mut::<tcp::Socket>(handle).close();
            }
            g.begin_close(handle, crate::netstack::SockKind::Tcp);
        }
        self.table().delete(rep)?;
        Ok(())
    }

    // ---- socket options: accepted but inert on the virtual fabric ----
    fn set_listen_backlog_size(
        &mut self,
        _: Resource<TcpSock>,
        _v: u64,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn keep_alive_enabled(
        &mut self,
        _: Resource<TcpSock>,
    ) -> Result<std::result::Result<bool, ErrorCode>> {
        Ok(Ok(false))
    }
    fn set_keep_alive_enabled(
        &mut self,
        _: Resource<TcpSock>,
        _v: bool,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn keep_alive_idle_time(
        &mut self,
        _: Resource<TcpSock>,
    ) -> Result<std::result::Result<u64, ErrorCode>> {
        Ok(Ok(0))
    }
    fn set_keep_alive_idle_time(
        &mut self,
        _: Resource<TcpSock>,
        _v: u64,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn keep_alive_interval(
        &mut self,
        _: Resource<TcpSock>,
    ) -> Result<std::result::Result<u64, ErrorCode>> {
        Ok(Ok(0))
    }
    fn set_keep_alive_interval(
        &mut self,
        _: Resource<TcpSock>,
        _v: u64,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn keep_alive_count(
        &mut self,
        _: Resource<TcpSock>,
    ) -> Result<std::result::Result<u32, ErrorCode>> {
        Ok(Ok(0))
    }
    fn set_keep_alive_count(
        &mut self,
        _: Resource<TcpSock>,
        _v: u32,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn hop_limit(&mut self, _: Resource<TcpSock>) -> Result<std::result::Result<u8, ErrorCode>> {
        Ok(Ok(64))
    }
    fn set_hop_limit(
        &mut self,
        _: Resource<TcpSock>,
        _v: u8,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn receive_buffer_size(
        &mut self,
        _: Resource<TcpSock>,
    ) -> Result<std::result::Result<u64, ErrorCode>> {
        Ok(Ok(TCP_BUF as u64))
    }
    fn set_receive_buffer_size(
        &mut self,
        _: Resource<TcpSock>,
        _v: u64,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
    fn send_buffer_size(
        &mut self,
        _: Resource<TcpSock>,
    ) -> Result<std::result::Result<u64, ErrorCode>> {
        Ok(Ok(TCP_BUF as u64))
    }
    fn set_send_buffer_size(
        &mut self,
        _: Resource<TcpSock>,
        _v: u64,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        Ok(Ok(()))
    }
}

// ---- ip-name-lookup (numeric literals only) ----

impl wasi::sockets::ip_name_lookup::Host for HostState {
    fn resolve_addresses(
        &mut self,
        _network: Resource<Net>,
        name: String,
    ) -> Result<std::result::Result<Resource<ResolveStream>, ErrorCode>> {
        let mut addrs = std::collections::VecDeque::new();
        let push_v4 = |addrs: &mut std::collections::VecDeque<IpAddress>,
                       v4: std::net::Ipv4Addr| {
            let o = v4.octets();
            addrs.push_back(IpAddress::Ipv4((o[0], o[1], o[2], o[3])));
        };
        let push_v6 = |addrs: &mut std::collections::VecDeque<IpAddress>,
                       v6: std::net::Ipv6Addr| {
            let s = v6.segments();
            addrs.push_back(IpAddress::Ipv6((
                s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
            )));
        };
        if let Ok(v4) = name.parse::<std::net::Ipv4Addr>() {
            push_v4(&mut addrs, v4);
        } else if let Ok(v6) = name.parse::<std::net::Ipv6Addr>() {
            push_v6(&mut addrs, v6);
        } else if let Some((net, hub)) = self
            .net
            .as_ref()
            .map(|n| (n.stack.lock().unwrap().net, n.hub.clone()))
            .filter(|(net, hub)| hub.resolve(*net, &name).is_some())
        {
            // A peer node on this node's virtual network, resolved by name —
            // offer both its IPv6 and IPv4 fabric addresses.
            if let Some(v6) = hub.resolve6(net, &name) {
                push_v6(&mut addrs, v6);
            }
            if let Some(v4) = hub.resolve(net, &name) {
                push_v4(&mut addrs, v4);
            }
        } else if self
            .net
            .as_ref()
            .is_some_and(|n| n.stack.lock().unwrap().host_access)
        {
            // Gatewayed node: resolve real names via the host resolver (v4 + v6).
            match std::net::ToSocketAddrs::to_socket_addrs(&(name.as_str(), 0)) {
                Ok(iter) => {
                    for sa in iter {
                        match sa.ip() {
                            std::net::IpAddr::V4(v4) => push_v4(&mut addrs, v4),
                            std::net::IpAddr::V6(v6) => push_v6(&mut addrs, v6),
                        }
                    }
                }
                Err(_) => return Ok(Err(ErrorCode::NameUnresolvable)),
            }
        } else {
            // No gateway: only numeric addresses resolve on the fabric.
            return Ok(Err(ErrorCode::NameUnresolvable));
        }
        Ok(Ok(self.table().push(ResolveStream { addrs })?))
    }
}

impl wasi::sockets::ip_name_lookup::HostResolveAddressStream for HostState {
    fn resolve_next_address(
        &mut self,
        this: Resource<ResolveStream>,
    ) -> Result<std::result::Result<Option<IpAddress>, ErrorCode>> {
        Ok(Ok(self.table().get_mut(&this)?.addrs.pop_front()))
    }
    fn subscribe(&mut self, _this: Resource<ResolveStream>) -> Result<Resource<DynPollable>> {
        let p = self.table().push(ReadyNow)?;
        subscribe(self.table(), p)
    }
    fn drop(&mut self, rep: Resource<ResolveStream>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

// ---- UDP over smoltcp (same fabric routing as TCP) ----

fn udp_packet_buffer() -> udp::PacketBuffer<'static> {
    udp::PacketBuffer::new(
        vec![udp::PacketMetadata::EMPTY; UDP_SLOTS],
        vec![0u8; UDP_BUF],
    )
}

impl wasi::sockets::udp_create_socket::Host for HostState {
    fn create_udp_socket(
        &mut self,
        family: IpAddressFamily,
    ) -> Result<std::result::Result<Resource<UdpSock>, ErrorCode>> {
        let Some(stack) = self.stack() else {
            return Ok(Err(ErrorCode::AccessDenied));
        };
        let handle = {
            let sock = udp::Socket::new(udp_packet_buffer(), udp_packet_buffer());
            let mut g = stack.lock().unwrap();
            let h = g.sockets.add(sock);
            g.track(h);
            h
        };
        Ok(Ok(self.table().push(UdpSock {
            handle,
            family,
            bound_port: 0,
            local: None,
            remote: None,
            bound: false,
        })?))
    }
}

impl wasi::sockets::udp::Host for HostState {}

impl wasi::sockets::udp::HostUdpSocket for HostState {
    fn start_bind(
        &mut self,
        this: Resource<UdpSock>,
        _network: Resource<Net>,
        local_address: IpSocketAddress,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let (_, port) = to_smol(local_address);
        let s = self.table().get_mut(&this)?;
        if s.bound {
            return Ok(Err(ErrorCode::InvalidState));
        }
        s.bound_port = port;
        s.local = Some(local_address);
        Ok(Ok(()))
    }
    fn finish_bind(
        &mut self,
        this: Resource<UdpSock>,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        let Some(stack) = self.stack() else {
            return Ok(Err(ErrorCode::AccessDenied));
        };
        // The bound family follows the address given to start-bind (default v4).
        let (handle, mut port, already, family) = {
            let s = self.table().get(&this)?;
            let family = match s.local {
                Some(IpSocketAddress::Ipv6(_)) => IpAddressFamily::Ipv6,
                _ => IpAddressFamily::Ipv4,
            };
            (s.handle, s.bound_port, s.bound, family)
        };
        if already {
            return Ok(Err(ErrorCode::InvalidState));
        }
        if port == 0 {
            let ctx = self.net.as_mut().unwrap();
            port = ctx.next_port;
            ctx.next_port = ctx.next_port.checked_add(1).unwrap_or(49152);
        }
        {
            let mut g = stack.lock().unwrap();
            if g.sockets.get_mut::<udp::Socket>(handle).bind(port).is_err() {
                return Ok(Err(ErrorCode::AddressInUse));
            }
        }
        let local_ip: smoltcp::wire::IpAddress = {
            let g = stack.lock().unwrap();
            match family {
                IpAddressFamily::Ipv6 => g.ip6.into(),
                IpAddressFamily::Ipv4 => g.ip.into(),
            }
        };
        let s = self.table().get_mut(&this)?;
        s.bound = true;
        s.bound_port = port;
        s.local = Some(from_smol(local_ip, port));
        Ok(Ok(()))
    }
    fn stream(
        &mut self,
        this: Resource<UdpSock>,
        remote_address: Option<IpSocketAddress>,
    ) -> Result<std::result::Result<(Resource<Datagrams>, Resource<Datagrams>), ErrorCode>> {
        let Some(stack) = self.stack() else {
            return Ok(Err(ErrorCode::AccessDenied));
        };
        let (handle, bound) = {
            let s = self.table().get(&this)?;
            (s.handle, s.bound)
        };
        if !bound {
            return Ok(Err(ErrorCode::InvalidState));
        }
        let remote = remote_address;
        self.table().get_mut(&this)?.remote = remote;
        let incoming = self.table().push(Datagrams {
            stack: stack.clone(),
            handle,
            remote,
        })?;
        let outgoing = self.table().push(Datagrams {
            stack,
            handle,
            remote,
        })?;
        Ok(Ok((incoming, outgoing)))
    }
    fn local_address(
        &mut self,
        this: Resource<UdpSock>,
    ) -> Result<std::result::Result<IpSocketAddress, ErrorCode>> {
        match self.table().get(&this)?.local {
            Some(a) => Ok(Ok(a)),
            None => Ok(Err(ErrorCode::InvalidState)),
        }
    }
    fn remote_address(
        &mut self,
        this: Resource<UdpSock>,
    ) -> Result<std::result::Result<IpSocketAddress, ErrorCode>> {
        match self.table().get(&this)?.remote {
            Some(a) => Ok(Ok(a)),
            None => Ok(Err(ErrorCode::InvalidState)),
        }
    }
    fn address_family(&mut self, this: Resource<UdpSock>) -> Result<IpAddressFamily> {
        Ok(self.table().get(&this)?.family)
    }
    fn unicast_hop_limit(
        &mut self,
        _: Resource<UdpSock>,
    ) -> Result<std::result::Result<u8, ErrorCode>> {
        Ok(Ok(64))
    }
    fn set_unicast_hop_limit(
        &mut self,
        _: Resource<UdpSock>,
        value: u8,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        if value == 0 {
            return Ok(Err(ErrorCode::InvalidArgument));
        }
        Ok(Ok(()))
    }
    fn receive_buffer_size(
        &mut self,
        _: Resource<UdpSock>,
    ) -> Result<std::result::Result<u64, ErrorCode>> {
        Ok(Ok(UDP_BUF as u64))
    }
    fn set_receive_buffer_size(
        &mut self,
        _: Resource<UdpSock>,
        value: u64,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        if value == 0 {
            return Ok(Err(ErrorCode::InvalidArgument));
        }
        Ok(Ok(()))
    }
    fn send_buffer_size(
        &mut self,
        _: Resource<UdpSock>,
    ) -> Result<std::result::Result<u64, ErrorCode>> {
        Ok(Ok(UDP_BUF as u64))
    }
    fn set_send_buffer_size(
        &mut self,
        _: Resource<UdpSock>,
        value: u64,
    ) -> Result<std::result::Result<(), ErrorCode>> {
        if value == 0 {
            return Ok(Err(ErrorCode::InvalidArgument));
        }
        Ok(Ok(()))
    }
    fn subscribe(&mut self, this: Resource<UdpSock>) -> Result<Resource<DynPollable>> {
        let stack = self.stack().expect("socket exists => has a stack");
        let handle = self.table().get(&this)?.handle;
        let p = self.table().push(UdpStreamPollable {
            stack,
            handle,
            send: true,
        })?;
        subscribe(self.table(), p)
    }
    fn drop(&mut self, rep: Resource<UdpSock>) -> Result<()> {
        // Reap the smoltcp socket so its buffers are freed (the datagram streams
        // only reference this handle and do no I/O once the socket is dropped).
        if let Some(stack) = self.stack() {
            let handle = self.table().get(&rep)?.handle;
            stack
                .lock()
                .unwrap()
                .begin_close(handle, crate::netstack::SockKind::Udp);
        }
        self.table().delete(rep)?;
        Ok(())
    }
}

impl wasi::sockets::udp::HostIncomingDatagramStream for HostState {
    fn receive(
        &mut self,
        this: Resource<Datagrams>,
        max_results: u64,
    ) -> Result<std::result::Result<Vec<wasi::sockets::udp::IncomingDatagram>, ErrorCode>> {
        let (stack, handle, filter) = {
            let d = self.table().get(&this)?;
            (d.stack.clone(), d.handle, d.remote)
        };
        let mut out = Vec::new();
        let mut g = stack.lock().unwrap();
        if !g.is_live(handle) {
            return Ok(Ok(out));
        }
        let s = g.sockets.get_mut::<udp::Socket>(handle);
        while (out.len() as u64) < max_results {
            let (data, src) = match s.recv() {
                Ok((data, meta)) => (
                    data.to_vec(),
                    from_smol(meta.endpoint.addr, meta.endpoint.port),
                ),
                Err(_) => break, // receive buffer exhausted
            };
            // A stream bound to a specific peer only yields that peer's datagrams.
            if let Some(f) = filter {
                if to_smol(f) != to_smol(src) {
                    continue;
                }
            }
            out.push(wasi::sockets::udp::IncomingDatagram {
                data,
                remote_address: src,
            });
        }
        Ok(Ok(out))
    }
    fn subscribe(&mut self, this: Resource<Datagrams>) -> Result<Resource<DynPollable>> {
        let (stack, handle) = {
            let d = self.table().get(&this)?;
            (d.stack.clone(), d.handle)
        };
        let p = self.table().push(UdpStreamPollable {
            stack,
            handle,
            send: false,
        })?;
        subscribe(self.table(), p)
    }
    fn drop(&mut self, rep: Resource<Datagrams>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

impl wasi::sockets::udp::HostOutgoingDatagramStream for HostState {
    fn check_send(
        &mut self,
        this: Resource<Datagrams>,
    ) -> Result<std::result::Result<u64, ErrorCode>> {
        let (stack, handle) = {
            let d = self.table().get(&this)?;
            (d.stack.clone(), d.handle)
        };
        let g = stack.lock().unwrap();
        if !g.is_live(handle) {
            return Ok(Ok(0));
        }
        let can = g.sockets.get::<udp::Socket>(handle).can_send();
        Ok(Ok(if can { UDP_SLOTS as u64 } else { 0 }))
    }
    fn send(
        &mut self,
        this: Resource<Datagrams>,
        datagrams: Vec<wasi::sockets::udp::OutgoingDatagram>,
    ) -> Result<std::result::Result<u64, ErrorCode>> {
        let (stack, handle, default_remote) = {
            let d = self.table().get(&this)?;
            (d.stack.clone(), d.handle, d.remote)
        };
        let mut g = stack.lock().unwrap();
        if !g.is_live(handle) {
            return Ok(Ok(0));
        }
        let s = g.sockets.get_mut::<udp::Socket>(handle);
        let mut sent = 0u64;
        for dg in datagrams {
            // Per-datagram destination, or the stream's default peer.
            let dest = match dg.remote_address.or(default_remote) {
                Some(a) => a,
                None => {
                    if sent == 0 {
                        return Ok(Err(ErrorCode::InvalidArgument));
                    }
                    break;
                }
            };
            let (ip, port) = to_smol(dest);
            if s.send_slice(&dg.data, (ip, port)).is_err() {
                break; // send buffer full: stop, report what we queued
            }
            sent += 1;
        }
        Ok(Ok(sent))
    }
    fn subscribe(&mut self, this: Resource<Datagrams>) -> Result<Resource<DynPollable>> {
        let (stack, handle) = {
            let d = self.table().get(&this)?;
            (d.stack.clone(), d.handle)
        };
        let p = self.table().push(UdpStreamPollable {
            stack,
            handle,
            send: true,
        })?;
        subscribe(self.table(), p)
    }
    fn drop(&mut self, rep: Resource<Datagrams>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}
