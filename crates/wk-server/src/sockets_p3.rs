//! wk's own `wasi:sockets@0.3` host implementation, over the same userspace
//! network fabric as the 0.2 impl in [`crate::sockets`] — closing the gap
//! where wasip3 guests got wasmtime's host-OS sockets (deny-all under wk)
//! instead of their node's smoltcp stack on the fabric.
//!
//! Shape: 0.3 drops the 0.2 start/finish dances and `wasi:io` streams for
//! component-model-native async — `connect` is an async function awaited to
//! completion, `listen` returns a `stream<tcp-socket>` of accepted
//! connections, and TCP bytes ride `stream<u8>` halves with a
//! `future<result<_, error-code>>` for the outcome. The socket state here
//! wraps the same fabric primitives the 0.2 impl uses (smoltcp handles on the
//! node's [`SharedStack`], generation-checked against slot recycling, the
//! Gateway-gated [`HostConn`] bridge for off-fabric destinations), so both
//! WASI generations see identical networks. Readiness is wired through
//! [`wk_fabric::netstack::NodeStack::park`]: producers/consumers register the
//! task waker on the node's stack and the hub's ~1 kHz tick wakes them to
//! re-check.

use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use tokio::sync::oneshot;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, HasData, Linker, Resource, ResourceTable, Source,
    StreamConsumer, StreamProducer, StreamReader, StreamResult,
};
use wasmtime::StoreContextMut;

use crate::sockets::{
    fabric_tcp_socket, local_ip_for, on_fabric, resolve_name, udp_packet_buffer, ConnReady,
    HostConn, NetCtx, SharedPipe, UdpReady, Want, WantReady, HOST_BUF_CAP, LISTEN_BACKLOG, TCP_BUF,
    UDP_BUF,
};
use wk_fabric::netstack::{NodeStack, SharedStack, SockKind};

wasmtime::component::bindgen!({
    path: "wit-sockets-p3",
    world: "sockets-host-p3",
    imports: {
        "wasi:sockets/types.[method]tcp-socket.listen": store | trappable,
        "wasi:sockets/types.[method]tcp-socket.send": store | trappable,
        "wasi:sockets/types.[method]tcp-socket.receive": store | trappable,
        default: trappable,
    },
    with: {
        "wasi:sockets/types.tcp-socket": crate::sockets_p3::TcpSocket,
        "wasi:sockets/types.udp-socket": crate::sockets_p3::UdpSocket,
    },
    trappable_error_type: {
        "wasi:sockets/types.error-code" => crate::sockets_p3::SocketsError,
    },
    require_store_data_send: true,
});

use wasi::sockets::ip_name_lookup;
pub use wasi::sockets::types;
use wasi::sockets::types::{ErrorCode, IpAddress, IpAddressFamily, IpSocketAddress};

pub type SocketsError = wasmtime_wasi::TrappableError<ErrorCode>;
pub type SocketsResult<T> = Result<T, SocketsError>;

/// Host-chosen chunk size when the guest's read buffer size is unknown.
const DEFAULT_BUFFER_CAPACITY: usize = 8192;

/// The largest payload a single UDP datagram can carry (IPv4 semantics; the
/// spec's "theoretical maximum length").
const MAX_UDP_PAYLOAD: usize = 65507;

/// What the embedder's store must provide for wk's `wasi:sockets@0.3`: the
/// resource table and the node's fabric network context (`None` means the
/// node has no network — socket creation is refused, same as 0.2).
pub trait NetView: Send {
    fn table(&mut self) -> &mut ResourceTable;
    fn net(&mut self) -> Option<&mut NetCtx>;
}

impl NetView for crate::plugin::HostState {
    fn table(&mut self) -> &mut ResourceTable {
        wasmtime_wasi_io::IoView::table(self)
    }
    fn net(&mut self) -> Option<&mut NetCtx> {
        self.net.as_mut()
    }
}

/// Adapter carrying the generated trait impls for any [`NetView`] store (the
/// same local-wrapper shape as `wk_vfs::VfsImpl`).
#[repr(transparent)]
pub struct SockImpl<T>(pub T);

pub struct HasNet<T>(std::marker::PhantomData<T>);
impl<T: NetView + 'static> HasData for HasNet<T> {
    type Data<'a> = SockImpl<&'a mut T>;
}

/// Add wk's `wasi:sockets@0.3` to the linker, alongside (not instead of) the
/// 0.2 fabric sockets — a guest compiled against either generation sees the
/// same virtual networks, addresses, and Gateway gating.
pub fn add_to_linker<T: NetView + Send + 'static>(l: &mut Linker<T>) -> wasmtime::Result<()> {
    wasi::sockets::types::add_to_linker::<_, HasNet<T>>(l, |s| SockImpl(s))?;
    wasi::sockets::ip_name_lookup::add_to_linker::<_, HasNet<T>>(l, |s| SockImpl(s))?;
    Ok(())
}

/// The accept pool of a listening socket — shared between the resource (which
/// reaps it on drop) and the accept-stream producer (which drains and
/// replenishes it). See [`LISTEN_BACKLOG`] for why it's a pool.
type ListenPool = Arc<Mutex<Vec<(SocketHandle, u64)>>>;

/// A 0.3 TCP socket on the node's fabric stack. Same underlying state as the
/// 0.2 [`crate::sockets::TcpSock`], with the 0.2 start/finish flags replaced
/// by the 0.3 operational states (bound / connecting / connected / listening
/// / closed) and the accept backlog moved into a [`ListenPool`].
pub struct TcpSocket {
    handle: SocketHandle,
    /// Generation of `handle` at creation, to detect slot recycling (see
    /// `NodeStack::is_current`). Copied onto derived stream halves.
    gen: u64,
    stack: SharedStack,
    family: IpAddressFamily,
    bound: bool,
    bound_port: u16,
    local: Option<IpSocketAddress>,
    remote: Option<IpSocketAddress>,
    listening: bool,
    connecting: bool,
    connected: bool,
    /// A failed connect leaves the socket dead; only `drop` remains valid.
    closed: bool,
    /// `Some` once listening; the live listener handles (primary + backlog).
    pool: Option<ListenPool>,
    /// Set when the socket connects off-fabric through a host gateway; its
    /// bytes flow over a real host socket instead of smoltcp.
    host: Option<HostConn>,
    /// `send` / `receive` may each be called at most once per socket.
    sent: bool,
    received: bool,
}

/// A 0.3 UDP socket on the node's fabric stack (the 0.2
/// [`crate::sockets::UdpSock`] state, minus the datagram-stream resources —
/// 0.3 sends and receives directly with async functions).
pub struct UdpSocket {
    handle: SocketHandle,
    gen: u64,
    stack: SharedStack,
    family: IpAddressFamily,
    bound: bool,
    bound_port: u16,
    local: Option<IpSocketAddress>,
    /// Default peer set by `connect` (POSIX "connected"): `send` is limited
    /// to it and `receive` filters to datagrams from it.
    remote: Option<IpSocketAddress>,
}

/// Adapt a `ResourceTable` error into a trap (the guest misused a handle).
fn tbl<V>(r: Result<V, wasmtime::component::ResourceTableError>) -> SocketsResult<V> {
    r.map_err(SocketsError::trap)
}

/// A 0.3 socket address -> a smoltcp address + port. The 0.2 twin lives in
/// [`crate::sockets`]; the generated address types are distinct per WASI
/// generation, so each module carries its own converters.
fn to_smol3(a: IpSocketAddress) -> (smoltcp::wire::IpAddress, u16) {
    match a {
        IpSocketAddress::Ipv4(s) => {
            let (a0, a1, a2, a3) = s.address;
            (
                smoltcp::wire::Ipv4Address::new(a0, a1, a2, a3).into(),
                s.port,
            )
        }
        IpSocketAddress::Ipv6(s) => {
            let g = s.address;
            let v6 = std::net::Ipv6Addr::new(g.0, g.1, g.2, g.3, g.4, g.5, g.6, g.7);
            (v6.into(), s.port)
        }
    }
}

/// A smoltcp address + port -> a 0.3 socket address.
fn from_smol3(ip: smoltcp::wire::IpAddress, port: u16) -> IpSocketAddress {
    match ip {
        smoltcp::wire::IpAddress::Ipv4(v4) => {
            let o = v4.octets();
            IpSocketAddress::Ipv4(types::Ipv4SocketAddress {
                port,
                address: (o[0], o[1], o[2], o[3]),
            })
        }
        smoltcp::wire::IpAddress::Ipv6(v6) => {
            let s = v6.segments();
            IpSocketAddress::Ipv6(types::Ipv6SocketAddress {
                port,
                flow_info: 0,
                address: (s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]),
                scope_id: 0,
            })
        }
    }
}

fn family_of(a: IpSocketAddress) -> IpAddressFamily {
    match a {
        IpSocketAddress::Ipv4(_) => IpAddressFamily::Ipv4,
        IpSocketAddress::Ipv6(_) => IpAddressFamily::Ipv6,
    }
}

/// The 0.3 bindgen's address family as the fabric's [`IpFamily`] (the 0.3
/// world generates its own `IpAddressFamily` type, distinct from 0.2's).
fn ip_family(f: IpAddressFamily) -> wk_fabric::netstack::IpFamily {
    match f {
        IpAddressFamily::Ipv4 => wk_fabric::netstack::IpFamily::V4,
        IpAddressFamily::Ipv6 => wk_fabric::netstack::IpFamily::V6,
    }
}

/// Does `addr`'s family match the wasi family a socket was created with?
/// (See `crate::sockets::family_matches` — same guarantee for the 0.3 impl.)
fn family_matches(addr: smoltcp::wire::IpAddress, family: IpAddressFamily) -> bool {
    wk_fabric::netstack::IpFamily::of(addr) == ip_family(family)
}

fn ip3(ip: std::net::IpAddr) -> IpAddress {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            IpAddress::Ipv4((o[0], o[1], o[2], o[3]))
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddress::Ipv6((s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]))
        }
    }
}

impl<T: NetView> SockImpl<&mut T> {
    fn table(&mut self) -> &mut ResourceTable {
        self.0.table()
    }
    fn net(&mut self) -> Option<&mut NetCtx> {
        self.0.net()
    }
    /// Allocate an ephemeral local port; sockets exist only when the store has
    /// a [`NetCtx`], so this cannot fail in practice.
    fn alloc_port(&mut self) -> SocketsResult<u16> {
        self.net()
            .map(|n| n.alloc_port())
            .ok_or_else(|| SocketsError::from(ErrorCode::AccessDenied))
    }

    /// Bind a UDP socket's smoltcp side to `port` (0 = ephemeral) and record
    /// the local address, exactly like the 0.2 finish-bind.
    fn udp_bind(&mut self, socket: &Resource<UdpSocket>, mut port: u16) -> SocketsResult<()> {
        let (stack, handle, family) = {
            let s = tbl(self.table().get(socket))?;
            (s.stack.clone(), s.handle, s.family)
        };
        if port == 0 {
            port = self.alloc_port()?;
        }
        {
            let mut g = stack.lock().unwrap();
            if g.sockets.get_mut::<udp::Socket>(handle).bind(port).is_err() {
                return Err(ErrorCode::AddressInUse.into());
            }
        }
        let local_ip: smoltcp::wire::IpAddress = {
            let g = stack.lock().unwrap();
            match family {
                IpAddressFamily::Ipv6 => g.ip6.into(),
                IpAddressFamily::Ipv4 => g.ip.into(),
            }
        };
        let s = tbl(self.table().get_mut(socket))?;
        s.bound = true;
        s.bound_port = port;
        s.local = Some(from_smol3(local_ip, port));
        Ok(())
    }
}

// ---- TCP stream halves over the fabric ----

/// Sends accepted connections as a `stream<tcp-socket>`; wakes via the hub
/// tick ([`NodeStack::park`]) when no pool socket has a peer yet.
struct AcceptProducer<T> {
    stack: SharedStack,
    port: u16,
    family: IpAddressFamily,
    pool: ListenPool,
    getter: for<'a> fn(&'a mut T) -> SockImpl<&'a mut T>,
}

impl<T: NetView + 'static> StreamProducer<T> for AcceptProducer<T> {
    type Item = Resource<TcpSocket>;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, T>,
        mut dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // A zero-length read is a readiness probe; like wasmtime's own p3
        // listener we claim readiness rather than block the probe (see
        // WebAssembly/component-model#561).
        if dst.remaining(&mut store) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let me = &*self;
        let (conn_handle, conn_gen, local, remote) = {
            let mut g = me.stack.lock().unwrap();
            let mut pool = me.pool.lock().unwrap();
            // Every listener gone (the listening resource was dropped): the
            // perpetual accept stream ends.
            if pool.iter().all(|&(h, gen)| !g.is_current(h, gen)) {
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            // The endpoint a consumed listener was armed with — its
            // connection's local address (every pool listener is armed with a
            // concrete family-scoped address; see `listen`).
            let relisten_ep = |g: &NodeStack, h: SocketHandle| -> smoltcp::wire::IpListenEndpoint {
                match g.sockets.get::<tcp::Socket>(h).local_endpoint() {
                    Some(ep) => smoltcp::wire::IpListenEndpoint {
                        addr: Some(ep.addr),
                        port: me.port,
                    },
                    None => g.listen_endpoints(ip_family(me.family), None, me.port)[0],
                }
            };
            // Defensive guarantee: never surface a peer of another family than
            // the listening socket's own (a guest libc may abort on the
            // mismatch). Family-scoped listeners make such a connection
            // impossible; if one ever appears, refuse it (RST) and re-arm.
            for (h, gen) in pool.iter_mut() {
                if !g.is_current(*h, *gen)
                    || matches!(
                        g.sockets.get::<tcp::Socket>(*h).state(),
                        tcp::State::Listen | tcp::State::SynSent | tcp::State::SynReceived
                    )
                    || g.sockets
                        .get::<tcp::Socket>(*h)
                        .remote_endpoint()
                        .is_none_or(|ep| family_matches(ep.addr, me.family))
                {
                    continue;
                }
                let ep = relisten_ep(&g, *h);
                g.sockets.get_mut::<tcp::Socket>(*h).abort();
                g.begin_close(*h, SockKind::Tcp);
                let fresh = g.sockets.add(fabric_tcp_socket());
                let fresh_gen = g.track(fresh);
                if g.sockets.get_mut::<tcp::Socket>(fresh).listen(ep).is_ok() {
                    (*h, *gen) = (fresh, fresh_gen);
                } else {
                    g.begin_close(fresh, SockKind::Tcp);
                }
            }
            // A peer has connected once a pool socket leaves the listening
            // handshake states (same predicate as 0.2's AcceptReady). Not
            // just `Established`: a fast peer may have already sent data and
            // a FIN, putting the socket in CloseWait with readable bytes —
            // still a valid accepted connection (the WIT notes an accepted
            // socket may even be closed before the server's first I/O).
            let idx = pool.iter().position(|&(h, gen)| {
                g.is_current(h, gen)
                    && !matches!(
                        g.sockets.get::<tcp::Socket>(h).state(),
                        tcp::State::Listen | tcp::State::SynSent | tcp::State::SynReceived
                    )
            });
            let Some(idx) = idx else {
                if finish {
                    return Poll::Ready(Ok(StreamResult::Cancelled));
                }
                g.park(cx.waker().clone());
                return Poll::Pending;
            };
            let (conn_handle, conn_gen) = pool[idx];
            // Capture the endpoints so get-local/remote-address work on the
            // accepted socket (getpeername — same reasoning as 0.2 accept).
            let sock = g.sockets.get::<tcp::Socket>(conn_handle);
            let local = sock.local_endpoint().map(|ep| from_smol3(ep.addr, ep.port));
            let remote = sock
                .remote_endpoint()
                .map(|ep| from_smol3(ep.addr, ep.port));
            // Replenish the consumed slot with a fresh listener on the same
            // family-scoped endpoint the consumed listener covered.
            let ep = relisten_ep(&g, conn_handle);
            let fresh = g.sockets.add(fabric_tcp_socket());
            let fresh_gen = g.track(fresh);
            if g.sockets.get_mut::<tcp::Socket>(fresh).listen(ep).is_ok() {
                pool[idx] = (fresh, fresh_gen);
            } else {
                g.begin_close(fresh, SockKind::Tcp);
                pool.remove(idx);
            }
            (conn_handle, conn_gen, local, remote)
        };
        let mut view = (me.getter)(store.data_mut());
        let resource = match view.table().push(TcpSocket {
            handle: conn_handle,
            gen: conn_gen,
            stack: me.stack.clone(),
            family: me.family,
            bound: true,
            bound_port: me.port,
            local,
            remote,
            listening: false,
            connecting: false,
            connected: true,
            closed: false,
            pool: None,
            host: None,
            sent: false,
            received: false,
        }) {
            Ok(r) => r,
            Err(e) => return Poll::Ready(Err(e.into())),
        };
        dst.set_buffer(Some(resource));
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// Drains a fabric TCP socket's receive buffer into the guest's `stream<u8>`;
/// parks on the stack when empty, closes (with the result future) on FIN.
struct TcpReceiveProducer {
    stack: SharedStack,
    handle: SocketHandle,
    gen: u64,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}

impl TcpReceiveProducer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for TcpReceiveProducer {
    fn drop(&mut self) {
        self.close(Ok(()));
    }
}

impl<D> StreamProducer<D> for TcpReceiveProducer {
    type Item = u8;
    type Buffer = wasmtime::component::VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'a, D>,
        dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut dst = dst.as_direct(store, DEFAULT_BUFFER_CAPACITY);
        let buf = dst.remaining();
        if buf.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let me = &mut *self;
        let mut g = me.stack.lock().unwrap();
        if !g.is_current(me.handle, me.gen) {
            // The owning resource dropped and the socket was reaped: end the
            // stream (same as 0.2, whose streams do no I/O after the drop).
            drop(g);
            me.close(Ok(()));
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        let (can, may) = {
            let s = g.sockets.get::<tcp::Socket>(me.handle);
            (s.can_recv(), s.may_recv())
        };
        if can {
            match g.sockets.get_mut::<tcp::Socket>(me.handle).recv_slice(buf) {
                Ok(n) => {
                    drop(g);
                    dst.mark_written(n);
                    Poll::Ready(Ok(StreamResult::Completed))
                }
                Err(_) => {
                    drop(g);
                    me.close(Err(ErrorCode::ConnectionReset));
                    Poll::Ready(Ok(StreamResult::Dropped))
                }
            }
        } else if may {
            if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                g.park(cx.waker().clone());
                Poll::Pending
            }
        } else {
            // Peer closed and the buffer is drained: a graceful end.
            drop(g);
            me.close(Ok(()));
            Poll::Ready(Ok(StreamResult::Dropped))
        }
    }
}

/// Pipes the guest's `stream<u8>` into a fabric TCP socket's send buffer;
/// parks on the stack when full. Dropping it (the guest closed its stream)
/// queues a FIN — `shutdown(SHUT_WR)`, per the WIT contract.
struct TcpSendConsumer {
    stack: SharedStack,
    handle: SocketHandle,
    gen: u64,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}

impl TcpSendConsumer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for TcpSendConsumer {
    fn drop(&mut self) {
        {
            let mut g = self.stack.lock().unwrap();
            if g.is_current(self.handle, self.gen) {
                g.sockets.get_mut::<tcp::Socket>(self.handle).close();
            }
        }
        self.close(Ok(()));
    }
}

impl<D> StreamConsumer<D> for TcpSendConsumer {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        src: Source<Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut src = src.as_direct(store);
        let me = &mut *self;
        let mut g = me.stack.lock().unwrap();
        if !g.is_current(me.handle, me.gen) {
            drop(g);
            me.close(Err(ErrorCode::ConnectionBroken));
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        let (can, may) = {
            let s = g.sockets.get::<tcp::Socket>(me.handle);
            (s.can_send(), s.may_send())
        };
        if !may {
            drop(g);
            me.close(Err(ErrorCode::ConnectionBroken));
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        if src.remaining().is_empty() {
            // A zero-length write is a readiness probe.
            return if can {
                Poll::Ready(Ok(StreamResult::Completed))
            } else if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                g.park(cx.waker().clone());
                Poll::Pending
            };
        }
        let n = g
            .sockets
            .get_mut::<tcp::Socket>(me.handle)
            .send_slice(src.remaining())
            .unwrap_or(0);
        if n == 0 {
            // Send buffer full: backpressure until the hub flushes some.
            if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                g.park(cx.waker().clone());
                Poll::Pending
            }
        } else {
            drop(g);
            src.mark_read(n);
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }
}

// ---- TCP stream halves over a host gateway bridge ----

/// `receive` for a Gateway-bridged connection: drains the host→guest pipe.
struct HostReceiveProducer {
    pipe: SharedPipe,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}

impl HostReceiveProducer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for HostReceiveProducer {
    fn drop(&mut self) {
        self.close(Ok(()));
    }
}

impl<D> StreamProducer<D> for HostReceiveProducer {
    type Item = u8;
    type Buffer = wasmtime::component::VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'a, D>,
        dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut dst = dst.as_direct(store, DEFAULT_BUFFER_CAPACITY);
        let buf = dst.remaining();
        if buf.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let me = &mut *self;
        let mut p = me.pipe.lock().unwrap();
        if p.buf.is_empty() {
            return if p.closed {
                drop(p);
                me.close(Ok(()));
                Poll::Ready(Ok(StreamResult::Dropped))
            } else if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                p.wakers.push(cx.waker().clone());
                Poll::Pending
            };
        }
        let n = buf.len().min(p.buf.len());
        for (slot, byte) in buf[..n].iter_mut().zip(p.buf.drain(..n)) {
            *slot = byte;
        }
        drop(p);
        dst.mark_written(n);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// `send` for a Gateway-bridged connection: fills the guest→host pipe,
/// backpressuring at [`HOST_BUF_CAP`]. Dropping it closes the pipe so the
/// pump shuts down the host socket's write side (SHUT_WR).
struct HostSendConsumer {
    pipe: SharedPipe,
    result: Option<oneshot::Sender<Result<(), ErrorCode>>>,
}

impl HostSendConsumer {
    fn close(&mut self, res: Result<(), ErrorCode>) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(res);
        }
    }
}

impl Drop for HostSendConsumer {
    fn drop(&mut self) {
        self.pipe.lock().unwrap().closed = true;
        self.close(Ok(()));
    }
}

impl<D> StreamConsumer<D> for HostSendConsumer {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        src: Source<Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut src = src.as_direct(store);
        let me = &mut *self;
        let mut p = me.pipe.lock().unwrap();
        if p.closed {
            drop(p);
            me.close(Err(ErrorCode::ConnectionBroken));
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        let room = HOST_BUF_CAP.saturating_sub(p.buf.len());
        if src.remaining().is_empty() {
            return if room > 0 {
                Poll::Ready(Ok(StreamResult::Completed))
            } else if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                p.wakers.push(cx.waker().clone());
                Poll::Pending
            };
        }
        if room == 0 {
            return if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                p.wakers.push(cx.waker().clone());
                Poll::Pending
            };
        }
        let n = room.min(src.remaining().len());
        p.buf.extend(&src.remaining()[..n]);
        drop(p);
        src.mark_read(n);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

// ---- the generated host traits ----

impl<T: NetView + Send> wasi::sockets::types::Host for SockImpl<&mut T> {
    fn convert_error_code(&mut self, err: SocketsError) -> wasmtime::Result<ErrorCode> {
        err.downcast()
    }
}

impl<T: NetView + Send> ip_name_lookup::Host for SockImpl<&mut T> {}

impl<T: NetView + Send + 'static> ip_name_lookup::HostWithStore<T> for HasNet<T> {
    async fn resolve_addresses(
        store: &Accessor<T, Self>,
        name: String,
    ) -> wasmtime::Result<Result<Vec<IpAddress>, ip_name_lookup::ErrorCode>> {
        store.with(|mut a| {
            let mut view = a.get();
            let resolved = {
                let net = view.net().map(|n| &*n);
                resolve_name(net, &name)
            };
            Ok(match resolved {
                Some(list) => Ok(list.into_iter().map(ip3).collect()),
                None => Err(ip_name_lookup::ErrorCode::NameUnresolvable),
            })
        })
    }
}

impl<T: NetView + Send> wasi::sockets::types::HostTcpSocket for SockImpl<&mut T> {
    fn create(&mut self, address_family: IpAddressFamily) -> SocketsResult<Resource<TcpSocket>> {
        // No fabric stack (the node has no network) => no sockets, exactly
        // like 0.2's create.
        let Some(net) = self.net() else {
            return Err(ErrorCode::AccessDenied.into());
        };
        let stack = net.stack.clone();
        let (handle, gen) = {
            let mut g = stack.lock().unwrap();
            let h = g.sockets.add(fabric_tcp_socket());
            let gen = g.track(h);
            (h, gen)
        };
        tbl(self.table().push(TcpSocket {
            handle,
            gen,
            stack,
            family: address_family,
            bound: false,
            bound_port: 0,
            local: None,
            remote: None,
            listening: false,
            connecting: false,
            connected: false,
            closed: false,
            pool: None,
            host: None,
            sent: false,
            received: false,
        }))
    }

    fn bind(
        &mut self,
        socket: Resource<TcpSocket>,
        local_address: IpSocketAddress,
    ) -> SocketsResult<()> {
        let (ip, mut port) = to_smol3(local_address);
        {
            let s = tbl(self.table().get(&socket))?;
            if s.bound || s.listening || s.connecting || s.connected || s.closed {
                return Err(ErrorCode::InvalidState.into());
            }
            if s.family != family_of(local_address) {
                return Err(ErrorCode::InvalidArgument.into());
            }
        }
        if port == 0 {
            port = self.alloc_port()?;
        }
        let s = tbl(self.table().get_mut(&socket))?;
        s.bound = true;
        s.bound_port = port;
        s.local = Some(from_smol3(ip, port));
        Ok(())
    }

    fn get_local_address(&mut self, socket: Resource<TcpSocket>) -> SocketsResult<IpSocketAddress> {
        tbl(self.table().get(&socket))?
            .local
            .ok_or_else(|| ErrorCode::InvalidState.into())
    }

    fn get_remote_address(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> SocketsResult<IpSocketAddress> {
        tbl(self.table().get(&socket))?
            .remote
            .ok_or_else(|| ErrorCode::InvalidState.into())
    }

    fn get_is_listening(&mut self, socket: Resource<TcpSocket>) -> wasmtime::Result<bool> {
        Ok(self.table().get(&socket)?.listening)
    }

    fn get_address_family(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> wasmtime::Result<IpAddressFamily> {
        Ok(self.table().get(&socket)?.family)
    }

    // ---- socket options: accepted but inert on the virtual fabric, the same
    // constants and behavior as the 0.2 impl ----
    fn set_listen_backlog_size(&mut self, _: Resource<TcpSocket>, _v: u64) -> SocketsResult<()> {
        Ok(())
    }
    fn get_keep_alive_enabled(&mut self, _: Resource<TcpSocket>) -> SocketsResult<bool> {
        Ok(false)
    }
    fn set_keep_alive_enabled(&mut self, _: Resource<TcpSocket>, _v: bool) -> SocketsResult<()> {
        Ok(())
    }
    fn get_keep_alive_idle_time(&mut self, _: Resource<TcpSocket>) -> SocketsResult<u64> {
        Ok(0)
    }
    fn set_keep_alive_idle_time(&mut self, _: Resource<TcpSocket>, _v: u64) -> SocketsResult<()> {
        Ok(())
    }
    fn get_keep_alive_interval(&mut self, _: Resource<TcpSocket>) -> SocketsResult<u64> {
        Ok(0)
    }
    fn set_keep_alive_interval(&mut self, _: Resource<TcpSocket>, _v: u64) -> SocketsResult<()> {
        Ok(())
    }
    fn get_keep_alive_count(&mut self, _: Resource<TcpSocket>) -> SocketsResult<u32> {
        Ok(0)
    }
    fn set_keep_alive_count(&mut self, _: Resource<TcpSocket>, _v: u32) -> SocketsResult<()> {
        Ok(())
    }
    fn get_hop_limit(&mut self, _: Resource<TcpSocket>) -> SocketsResult<u8> {
        Ok(64)
    }
    fn set_hop_limit(&mut self, _: Resource<TcpSocket>, _v: u8) -> SocketsResult<()> {
        Ok(())
    }
    fn get_receive_buffer_size(&mut self, _: Resource<TcpSocket>) -> SocketsResult<u64> {
        Ok(TCP_BUF as u64)
    }
    fn set_receive_buffer_size(&mut self, _: Resource<TcpSocket>, _v: u64) -> SocketsResult<()> {
        Ok(())
    }
    fn get_send_buffer_size(&mut self, _: Resource<TcpSocket>) -> SocketsResult<u64> {
        Ok(TCP_BUF as u64)
    }
    fn set_send_buffer_size(&mut self, _: Resource<TcpSocket>, _v: u64) -> SocketsResult<()> {
        Ok(())
    }

    fn drop(&mut self, rep: Resource<TcpSocket>) -> wasmtime::Result<()> {
        // Reap the smoltcp socket(s): queue a graceful FIN and hand them to
        // the hub to remove once drained — same as the 0.2 drop.
        let sock = self.table().delete(rep)?;
        let mut g = sock.stack.lock().unwrap();
        if let Some(pool) = &sock.pool {
            // A listener: its live handles are the pool (the primary slot may
            // have been replenished past `sock.handle` by accepts). Accepted
            // connections are independent resources and stay live.
            for &(h, hg) in pool.lock().unwrap().iter() {
                if g.is_current(h, hg) {
                    g.sockets.get_mut::<tcp::Socket>(h).close();
                    g.begin_close(h, SockKind::Tcp);
                }
            }
        } else {
            if g.is_current(sock.handle, sock.gen) {
                g.sockets.get_mut::<tcp::Socket>(sock.handle).close();
            }
            g.begin_close(sock.handle, SockKind::Tcp);
        }
        Ok(())
    }
}

/// How a connect settles: awaiting the fabric handshake or the gateway
/// bridge's host connect.
enum ConnectPath {
    Fabric(WantReady),
    Host {
        settled: ConnReady,
        failed: Arc<std::sync::atomic::AtomicBool>,
    },
}

impl<T: NetView + Send + 'static> wasi::sockets::types::HostTcpSocketWithStore<T> for HasNet<T> {
    async fn connect(
        store: &Accessor<T, Self>,
        socket: Resource<TcpSocket>,
        remote_address: IpSocketAddress,
    ) -> SocketsResult<()> {
        let (remote_ip, remote_port) = to_smol3(remote_address);
        if remote_port == 0 || remote_ip.is_unspecified() {
            return Err(ErrorCode::InvalidArgument.into());
        }
        let path = store.with(|mut a| -> SocketsResult<ConnectPath> {
            let mut view = a.get();
            let (stack, handle, gen, family) = {
                let s = tbl(view.table().get(&socket))?;
                if s.connected || s.connecting || s.listening || s.closed {
                    return Err(ErrorCode::InvalidState.into());
                }
                (s.stack.clone(), s.handle, s.gen, s.family)
            };
            if family != family_of(remote_address) {
                return Err(ErrorCode::InvalidArgument.into());
            }
            if !on_fabric(remote_ip) {
                // Off-fabric destination: bridge to the real host network,
                // but only if this node is wired to a Gateway (host access).
                if !stack.lock().unwrap().host_access {
                    return Err(ErrorCode::AccessDenied.into());
                }
                let conn = HostConn::connect(remote_ip, remote_port);
                let settled = ConnReady {
                    pipe: conn.incoming.clone(),
                    connected: conn.connected.clone(),
                    failed: conn.failed.clone(),
                };
                let failed = conn.failed.clone();
                let s = tbl(view.table().get_mut(&socket))?;
                s.host = Some(conn);
                s.remote = Some(remote_address);
                s.connecting = true;
                return Ok(ConnectPath::Host { settled, failed });
            }
            let bound_port = {
                let s = tbl(view.table().get(&socket))?;
                if s.bound {
                    s.bound_port
                } else {
                    0
                }
            };
            let lport = if bound_port != 0 {
                bound_port
            } else {
                view.alloc_port()?
            };
            let local_ip = local_ip_for(&stack, remote_ip);
            {
                let mut g = stack.lock().unwrap();
                let NodeStack { iface, sockets, .. } = &mut *g;
                let s = sockets.get_mut::<tcp::Socket>(handle);
                if s.connect(iface.context(), (remote_ip, remote_port), lport)
                    .is_err()
                {
                    return Err(ErrorCode::InvalidState.into());
                }
            }
            let s = tbl(view.table().get_mut(&socket))?;
            s.connecting = true;
            s.remote = Some(remote_address);
            s.local = Some(from_smol3(local_ip, lport));
            Ok(ConnectPath::Fabric(WantReady {
                stack,
                want: Want::Event(handle),
                gen,
            }))
        })?;
        match path {
            ConnectPath::Fabric(settled) => {
                settled.await;
                store.with(|mut a| {
                    let mut view = a.get();
                    let s = tbl(view.table().get_mut(&socket))?;
                    let established = {
                        let g = s.stack.lock().unwrap();
                        g.is_current(s.handle, s.gen)
                            && g.sockets.get::<tcp::Socket>(s.handle).state()
                                == tcp::State::Established
                    };
                    s.connecting = false;
                    if established {
                        s.connected = true;
                        Ok(())
                    } else {
                        s.closed = true;
                        s.remote = None;
                        Err(ErrorCode::ConnectionRefused.into())
                    }
                })
            }
            ConnectPath::Host { settled, failed } => {
                settled.await;
                store.with(|mut a| {
                    let mut view = a.get();
                    let s = tbl(view.table().get_mut(&socket))?;
                    s.connecting = false;
                    if failed.load(Ordering::Relaxed) {
                        s.closed = true;
                        s.host = None;
                        s.remote = None;
                        Err(ErrorCode::ConnectionRefused.into())
                    } else {
                        s.connected = true;
                        Ok(())
                    }
                })
            }
        }
    }

    fn listen(
        mut store: Access<'_, T, Self>,
        socket: Resource<TcpSocket>,
    ) -> SocketsResult<StreamReader<Resource<TcpSocket>>> {
        let getter = store.getter();
        let producer = {
            let mut view = store.get();
            let (stack, handle, gen, family, bound, bound_port, local) = {
                let s = tbl(view.table().get(&socket))?;
                if s.listening || s.connecting || s.connected || s.closed {
                    return Err(ErrorCode::InvalidState.into());
                }
                (
                    s.stack.clone(),
                    s.handle,
                    s.gen,
                    s.family,
                    s.bound,
                    s.bound_port,
                    s.local,
                )
            };
            // Implicit bind to an ephemeral port when not explicitly bound
            // (smoltcp rejects `listen(0)`, same as 0.2's finish-bind fixup).
            let port = if bound && bound_port != 0 {
                bound_port
            } else {
                view.alloc_port()?
            };
            let pool_vec = {
                let mut g = stack.lock().unwrap();
                // Family-scoped endpoints (see `NodeStack::listen_endpoints`):
                // a listener only matches its own family's local addresses, so
                // a v6 SYN to a v4-bound port is refused rather than surfacing
                // a v6 peer on a v4 socket — same as the 0.2 start-listen.
                let eps = g.listen_endpoints(ip_family(family), local.map(|a| to_smol3(a).0), port);
                if g.sockets
                    .get_mut::<tcp::Socket>(handle)
                    .listen(eps[0])
                    .is_err()
                {
                    return Err(ErrorCode::InvalidState.into());
                }
                // A pool of extra listeners on the same port so several peers
                // can connect at once — see LISTEN_BACKLOG in crate::sockets.
                let mut pool = Vec::with_capacity(LISTEN_BACKLOG);
                pool.push((handle, gen));
                for i in 1..LISTEN_BACKLOG {
                    let h = g.sockets.add(fabric_tcp_socket());
                    let hg = g.track(h);
                    if g.sockets
                        .get_mut::<tcp::Socket>(h)
                        .listen(eps[i % eps.len()])
                        .is_err()
                    {
                        g.begin_close(h, SockKind::Tcp);
                        continue;
                    }
                    pool.push((h, hg));
                }
                pool
            };
            let local_ip = local_ip_for(
                &stack,
                match family {
                    IpAddressFamily::Ipv4 => smoltcp::wire::IpAddress::v4(10, 0, 0, 1),
                    IpAddressFamily::Ipv6 => smoltcp::wire::Ipv6Address::LOCALHOST.into(),
                },
            );
            let pool: ListenPool = Arc::new(Mutex::new(pool_vec));
            {
                let s = tbl(view.table().get_mut(&socket))?;
                s.listening = true;
                s.bound = true;
                s.bound_port = port;
                if s.local.is_none() {
                    s.local = Some(from_smol3(local_ip, port));
                }
                s.pool = Some(pool.clone());
            }
            AcceptProducer {
                stack,
                port,
                family,
                pool,
                getter,
            }
        };
        StreamReader::new(&mut store, producer).map_err(SocketsError::trap)
    }

    fn send(
        mut store: Access<'_, T, Self>,
        socket: Resource<TcpSocket>,
        mut data: StreamReader<u8>,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        enum Target {
            Fabric(SharedStack, SocketHandle, u64),
            Host(SharedPipe),
        }
        let (tx, rx) = oneshot::channel();
        let target = {
            let mut view = store.get();
            let s = view.table().get_mut(&socket)?;
            if !s.connected || s.sent {
                Err(ErrorCode::InvalidState)
            } else {
                s.sent = true;
                match &s.host {
                    Some(h) => Ok(Target::Host(h.outgoing.clone())),
                    None => Ok(Target::Fabric(s.stack.clone(), s.handle, s.gen)),
                }
            }
        };
        match target {
            Ok(Target::Fabric(stack, handle, gen)) => data.pipe(
                &mut store,
                TcpSendConsumer {
                    stack,
                    handle,
                    gen,
                    result: Some(tx),
                },
            )?,
            Ok(Target::Host(pipe)) => data.pipe(
                &mut store,
                HostSendConsumer {
                    pipe,
                    result: Some(tx),
                },
            )?,
            Err(code) => {
                data.close(&mut store)?;
                let _ = tx.send(Err(code));
            }
        }
        FutureReader::new(&mut store, rx)
    }

    fn receive(
        mut store: Access<'_, T, Self>,
        socket: Resource<TcpSocket>,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)> {
        enum Src {
            Fabric(SharedStack, SocketHandle, u64),
            Host(SharedPipe),
        }
        let (tx, rx) = oneshot::channel();
        let src = {
            let mut view = store.get();
            let s = view.table().get_mut(&socket)?;
            if !s.connected || s.received {
                Err(ErrorCode::InvalidState)
            } else {
                s.received = true;
                match &s.host {
                    Some(h) => Ok(Src::Host(h.incoming.clone())),
                    None => Ok(Src::Fabric(s.stack.clone(), s.handle, s.gen)),
                }
            }
        };
        let stream = match src {
            Ok(Src::Fabric(stack, handle, gen)) => StreamReader::new(
                &mut store,
                TcpReceiveProducer {
                    stack,
                    handle,
                    gen,
                    result: Some(tx),
                },
            )?,
            Ok(Src::Host(pipe)) => StreamReader::new(
                &mut store,
                HostReceiveProducer {
                    pipe,
                    result: Some(tx),
                },
            )?,
            Err(code) => {
                let _ = tx.send(Err(code));
                StreamReader::new(&mut store, std::iter::empty())?
            }
        };
        Ok((stream, FutureReader::new(&mut store, rx)?))
    }
}

impl<T: NetView + Send> wasi::sockets::types::HostUdpSocket for SockImpl<&mut T> {
    fn create(&mut self, address_family: IpAddressFamily) -> SocketsResult<Resource<UdpSocket>> {
        let Some(net) = self.net() else {
            return Err(ErrorCode::AccessDenied.into());
        };
        let stack = net.stack.clone();
        let (handle, gen) = {
            let sock = udp::Socket::new(udp_packet_buffer(), udp_packet_buffer());
            let mut g = stack.lock().unwrap();
            let h = g.sockets.add(sock);
            let gen = g.track(h);
            (h, gen)
        };
        tbl(self.table().push(UdpSocket {
            handle,
            gen,
            stack,
            family: address_family,
            bound: false,
            bound_port: 0,
            local: None,
            remote: None,
        }))
    }

    fn bind(
        &mut self,
        socket: Resource<UdpSocket>,
        local_address: IpSocketAddress,
    ) -> SocketsResult<()> {
        let (_, port) = to_smol3(local_address);
        {
            let s = tbl(self.table().get(&socket))?;
            if s.bound {
                return Err(ErrorCode::InvalidState.into());
            }
            if s.family != family_of(local_address) {
                return Err(ErrorCode::InvalidArgument.into());
            }
        }
        self.udp_bind(&socket, port)
    }

    fn connect(
        &mut self,
        socket: Resource<UdpSocket>,
        remote_address: IpSocketAddress,
    ) -> SocketsResult<()> {
        let (ip, port) = to_smol3(remote_address);
        if port == 0 || ip.is_unspecified() {
            return Err(ErrorCode::InvalidArgument.into());
        }
        let bound = {
            let s = tbl(self.table().get(&socket))?;
            if s.family != family_of(remote_address) {
                return Err(ErrorCode::InvalidArgument.into());
            }
            s.bound
        };
        if !bound {
            self.udp_bind(&socket, 0)?;
        }
        tbl(self.table().get_mut(&socket))?.remote = Some(remote_address);
        Ok(())
    }

    fn disconnect(&mut self, socket: Resource<UdpSocket>) -> SocketsResult<()> {
        let s = tbl(self.table().get_mut(&socket))?;
        if s.remote.is_none() {
            return Err(ErrorCode::InvalidState.into());
        }
        s.remote = None;
        Ok(())
    }

    fn get_local_address(&mut self, socket: Resource<UdpSocket>) -> SocketsResult<IpSocketAddress> {
        tbl(self.table().get(&socket))?
            .local
            .ok_or_else(|| ErrorCode::InvalidState.into())
    }

    fn get_remote_address(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> SocketsResult<IpSocketAddress> {
        tbl(self.table().get(&socket))?
            .remote
            .ok_or_else(|| ErrorCode::InvalidState.into())
    }

    fn get_address_family(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> wasmtime::Result<IpAddressFamily> {
        Ok(self.table().get(&socket)?.family)
    }

    // ---- options: same constants and validation as the 0.2 impl ----
    fn get_unicast_hop_limit(&mut self, _: Resource<UdpSocket>) -> SocketsResult<u8> {
        Ok(64)
    }
    fn set_unicast_hop_limit(&mut self, _: Resource<UdpSocket>, value: u8) -> SocketsResult<()> {
        if value == 0 {
            return Err(ErrorCode::InvalidArgument.into());
        }
        Ok(())
    }
    fn get_receive_buffer_size(&mut self, _: Resource<UdpSocket>) -> SocketsResult<u64> {
        Ok(UDP_BUF as u64)
    }
    fn set_receive_buffer_size(&mut self, _: Resource<UdpSocket>, value: u64) -> SocketsResult<()> {
        if value == 0 {
            return Err(ErrorCode::InvalidArgument.into());
        }
        Ok(())
    }
    fn get_send_buffer_size(&mut self, _: Resource<UdpSocket>) -> SocketsResult<u64> {
        Ok(UDP_BUF as u64)
    }
    fn set_send_buffer_size(&mut self, _: Resource<UdpSocket>, value: u64) -> SocketsResult<()> {
        if value == 0 {
            return Err(ErrorCode::InvalidArgument.into());
        }
        Ok(())
    }

    fn drop(&mut self, rep: Resource<UdpSocket>) -> wasmtime::Result<()> {
        // Reap the smoltcp socket so its buffers are freed (0.2's drop).
        let sock = self.table().delete(rep)?;
        sock.stack
            .lock()
            .unwrap()
            .begin_close(sock.handle, SockKind::Udp);
        Ok(())
    }
}

impl<T: NetView + Send + 'static> wasi::sockets::types::HostUdpSocketWithStore<T> for HasNet<T> {
    async fn send(
        store: &Accessor<T, Self>,
        socket: Resource<UdpSocket>,
        data: Vec<u8>,
        remote_address: Option<IpSocketAddress>,
    ) -> SocketsResult<()> {
        if data.len() > MAX_UDP_PAYLOAD {
            return Err(ErrorCode::DatagramTooLarge.into());
        }
        type SendPlan = (
            SharedStack,
            SocketHandle,
            u64,
            (smoltcp::wire::IpAddress, u16),
        );
        let (stack, handle, gen, dest) = store.with(|mut a| -> SocketsResult<SendPlan> {
            let mut view = a.get();
            let bound = tbl(view.table().get(&socket))?.bound;
            if !bound {
                // WASI requires send to perform an implicit bind.
                view.udp_bind(&socket, 0)?;
            }
            let s = tbl(view.table().get(&socket))?;
            let dest = match (s.remote, remote_address) {
                // A connected socket only sends to its peer.
                (Some(peer), Some(given)) if to_smol3(given) != to_smol3(peer) => {
                    return Err(ErrorCode::InvalidArgument.into())
                }
                (Some(peer), _) => peer,
                (None, Some(given)) => given,
                (None, None) => return Err(ErrorCode::InvalidArgument.into()),
            };
            let (ip, port) = to_smol3(dest);
            if port == 0 || ip.is_unspecified() {
                return Err(ErrorCode::InvalidArgument.into());
            }
            Ok((s.stack.clone(), s.handle, s.gen, (ip, port)))
        })?;
        loop {
            // Wait for buffer space; the hub tick wakes the parked waker.
            UdpReady {
                // 0.3 has no UDP gateway bridge yet (see sockets.rs HostUdp).
                host: None,
                stack: stack.clone(),
                handle,
                gen,
                send: true,
            }
            .await;
            let mut g = stack.lock().unwrap();
            if !g.is_current(handle, gen) {
                return Err(ErrorCode::InvalidState.into());
            }
            match g
                .sockets
                .get_mut::<udp::Socket>(handle)
                .send_slice(&data, dest)
            {
                Ok(()) => return Ok(()),
                Err(udp::SendError::BufferFull) => continue,
                Err(_) => return Err(ErrorCode::InvalidArgument.into()),
            }
        }
    }

    async fn receive(
        store: &Accessor<T, Self>,
        socket: Resource<UdpSocket>,
    ) -> SocketsResult<(Vec<u8>, IpSocketAddress)> {
        type RecvPlan = (
            SharedStack,
            SocketHandle,
            u64,
            Option<IpSocketAddress>,
            IpAddressFamily,
        );
        let (stack, handle, gen, filter, family) =
            store.with(|mut a| -> SocketsResult<RecvPlan> {
                let mut view = a.get();
                let s = tbl(view.table().get(&socket))?;
                if !s.bound {
                    return Err(ErrorCode::InvalidState.into());
                }
                Ok((s.stack.clone(), s.handle, s.gen, s.remote, s.family))
            })?;
        loop {
            UdpReady {
                // 0.3 has no UDP gateway bridge yet (see sockets.rs HostUdp).
                host: None,
                stack: stack.clone(),
                handle,
                gen,
                send: false,
            }
            .await;
            let mut g = stack.lock().unwrap();
            if !g.is_current(handle, gen) {
                return Err(ErrorCode::InvalidState.into());
            }
            let s = g.sockets.get_mut::<udp::Socket>(handle);
            let Ok((data, meta)) = s.recv() else {
                continue; // spurious wake
            };
            // Never surface a peer of the other family (the guest's sockaddr
            // conversion may abort on it): drop the datagram, as a kernel
            // would never have delivered it to this socket at all.
            if !family_matches(meta.endpoint.addr, family) {
                continue;
            }
            let src = from_smol3(meta.endpoint.addr, meta.endpoint.port);
            // A connected socket only yields its peer's datagrams.
            if let Some(peer) = filter {
                if to_smol3(peer) != to_smol3(src) {
                    continue;
                }
            }
            return Ok((data.to_vec(), src));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::wire::Ipv4Address;
    use std::mem;
    use wasmtime::component::{Lift, ResourceTable};
    use wasmtime::{Config, Engine, Store};
    use wk_fabric::netstack::NetHub;
    use wk_protocol::NodeId;

    struct TestStore {
        table: ResourceTable,
        net: Option<NetCtx>,
    }
    impl NetView for TestStore {
        fn table(&mut self) -> &mut ResourceTable {
            &mut self.table
        }
        fn net(&mut self) -> Option<&mut NetCtx> {
            self.net.as_mut()
        }
    }

    fn engine() -> Engine {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        Engine::new(&config).expect("engine")
    }

    type Nacc = Accessor<TestStore, HasNet<TestStore>>;

    fn res<R: 'static>(r: &Resource<R>) -> Resource<R> {
        Resource::new_own(r.rep())
    }

    fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> IpSocketAddress {
        IpSocketAddress::Ipv4(wasi::sockets::types::Ipv4SocketAddress {
            port,
            address: (a, b, c, d),
        })
    }

    /// Unwrap a p3 sockets result in a test, showing the code on failure.
    fn must<V>(r: SocketsResult<V>) -> V {
        match r {
            Ok(v) => v,
            Err(e) => panic!("sockets error: {:?}", e.downcast()),
        }
    }

    /// The error code carried by a failed result (traps panic the test).
    fn code<V>(r: SocketsResult<V>) -> ErrorCode {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.downcast().expect("error code, not a trap"),
        }
    }

    /// Collects a `stream<u8>` host-side; sends the bytes on drop (the pipe
    /// machinery drops the consumer when the producer's stream ends).
    struct Collect {
        data: Vec<u8>,
        tx: Option<oneshot::Sender<Vec<u8>>>,
    }
    impl<D> StreamConsumer<D> for Collect {
        type Item = u8;
        fn poll_consume(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            store: StoreContextMut<D>,
            src: Source<Self::Item>,
            _finish: bool,
        ) -> Poll<wasmtime::Result<StreamResult>> {
            let me = &mut *self;
            let mut src = src.as_direct(store);
            me.data.extend_from_slice(src.remaining());
            let n = src.remaining().len();
            src.mark_read(n);
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }
    impl Drop for Collect {
        fn drop(&mut self) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(mem::take(&mut self.data));
            }
        }
    }

    /// Takes the first item of a `stream<T>` (the accept stream) and ends.
    struct TakeOne<V: 'static> {
        tx: Option<oneshot::Sender<V>>,
    }
    impl<D, V: Lift + Send + Sync + Unpin + 'static> StreamConsumer<D> for TakeOne<V> {
        type Item = V;
        fn poll_consume(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            mut store: StoreContextMut<D>,
            mut src: Source<Self::Item>,
            _finish: bool,
        ) -> Poll<wasmtime::Result<StreamResult>> {
            let me = self.get_mut();
            let mut buf: Vec<V> = Vec::with_capacity(1);
            src.read(&mut store, &mut buf)?;
            if let Some(v) = buf.pop() {
                if let Some(tx) = me.tx.take() {
                    let _ = tx.send(v);
                }
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }

    /// Resolves a `future<T>` host-side into a oneshot.
    struct FutureRx<V: Send + Sync + 'static> {
        tx: Option<oneshot::Sender<V>>,
    }
    impl<D, V: Lift + Send + Sync + 'static> wasmtime::component::FutureConsumer<D> for FutureRx<V> {
        type Item = V;
        fn poll_consume(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            mut store: StoreContextMut<D>,
            mut source: Source<'_, Self::Item>,
            _finish: bool,
        ) -> Poll<wasmtime::Result<()>> {
            let mut buf: Option<V> = None;
            source.read(&mut store, &mut buf)?;
            if let (Some(v), Some(tx)) = (buf, self.tx.take()) {
                let _ = tx.send(v);
            }
            Poll::Ready(Ok(()))
        }
    }

    async fn await_result(
        nacc: &Nacc,
        fut: FutureReader<Result<(), ErrorCode>>,
    ) -> Result<(), ErrorCode> {
        let (tx, rx) = oneshot::channel();
        nacc.with(|mut a| fut.pipe(&mut a, FutureRx { tx: Some(tx) }))
            .expect("pipe result future");
        rx.await.expect("result future resolves")
    }

    async fn collect_stream(nacc: &Nacc, stream: StreamReader<u8>) -> Vec<u8> {
        let (tx, rx) = oneshot::channel();
        nacc.with(|mut a| {
            stream.pipe(
                &mut a,
                Collect {
                    data: Vec::new(),
                    tx: Some(tx),
                },
            )
        })
        .expect("pipe stream");
        rx.await.expect("stream collected")
    }

    fn store_on(engine: &Engine, ctx: Option<NetCtx>) -> Store<TestStore> {
        Store::new(
            engine,
            TestStore {
                table: ResourceTable::new(),
                net: ctx,
            },
        )
    }

    /// Full TCP flow across two nodes on one fabric net: bind + listen (accept
    /// stream), async connect, bytes both directions through the send/receive
    /// streams, clean close (both result futures resolve `Ok`).
    #[test]
    fn p3_tcp_two_nodes_exchange_bytes() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let hub = NetHub::new();
            let net = NodeId::nil();
            let client_stack = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "client");
            let server_stack = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "server");
            let engine = engine();
            let mut client_store = store_on(&engine, Some(NetCtx::new(client_stack, hub.clone())));
            let mut server_store = store_on(&engine, Some(NetCtx::new(server_stack, hub.clone())));

            let (listening_tx, listening_rx) = oneshot::channel::<()>();

            let server = server_store.run_concurrent(async move |acc| {
                use wasi::sockets::types::HostTcpSocket as H;
                use wasi::sockets::types::HostTcpSocketWithStore as S;
                let nacc: Nacc = acc.with_getter(|s| SockImpl(s));
                let sock = must(nacc.with(|mut a| H::create(&mut a.get(), IpAddressFamily::Ipv4)));
                must(nacc.with(|mut a| H::bind(&mut a.get(), res(&sock), v4(10, 0, 0, 2, 8080))));
                let accepts = must(nacc.with(|a| S::listen(a, res(&sock))));
                assert!(nacc
                    .with(|mut a| H::get_is_listening(&mut a.get(), res(&sock)))
                    .unwrap());
                listening_tx.send(()).expect("signal listening");

                // Take the first accepted connection off the stream.
                let (tx, rx) = oneshot::channel();
                nacc.with(|mut a| accepts.pipe(&mut a, TakeOne { tx: Some(tx) }))?;
                let conn: Resource<TcpSocket> = rx.await.expect("accepted connection");
                let peer = must(nacc.with(|mut a| H::get_remote_address(&mut a.get(), res(&conn))));
                assert!(matches!(peer, IpSocketAddress::Ipv4(p) if p.address == (10, 0, 0, 1)));

                // Client -> server bytes, then a graceful FIN ends the stream.
                let (stream, done) = nacc.with(|a| S::receive(a, res(&conn)))?;
                assert_eq!(collect_stream(&nacc, stream).await, b"ping");
                assert!(matches!(await_result(&nacc, done).await, Ok(())));

                // Server -> client reply.
                let input = nacc
                    .with(|mut a| StreamReader::new(&mut a, b"pong".to_vec().into_boxed_slice()))?;
                let done = nacc.with(|a| S::send(a, res(&conn), input))?;
                assert!(matches!(await_result(&nacc, done).await, Ok(())));
                wasmtime::error::Ok(())
            });

            let client = client_store.run_concurrent(async move |acc| {
                use wasi::sockets::types::HostTcpSocket as H;
                use wasi::sockets::types::HostTcpSocketWithStore as S;
                let nacc: Nacc = acc.with_getter(|s| SockImpl(s));
                listening_rx.await.expect("server listening");
                let sock = must(nacc.with(|mut a| H::create(&mut a.get(), IpAddressFamily::Ipv4)));
                must(S::connect(&nacc, res(&sock), v4(10, 0, 0, 2, 8080)).await);
                let local = must(nacc.with(|mut a| H::get_local_address(&mut a.get(), res(&sock))));
                assert!(matches!(local, IpSocketAddress::Ipv4(p) if p.address == (10, 0, 0, 1)));

                let input = nacc
                    .with(|mut a| StreamReader::new(&mut a, b"ping".to_vec().into_boxed_slice()))?;
                let done = nacc.with(|a| S::send(a, res(&sock), input))?;
                assert!(matches!(await_result(&nacc, done).await, Ok(())));

                let (stream, done) = nacc.with(|a| S::receive(a, res(&sock)))?;
                assert_eq!(collect_stream(&nacc, stream).await, b"pong");
                assert!(matches!(await_result(&nacc, done).await, Ok(())));
                wasmtime::error::Ok(())
            });

            let (sr, cr) = tokio::join!(server, client);
            sr.expect("server run_concurrent").expect("server body");
            cr.expect("client run_concurrent").expect("client body");
        });
    }

    /// UDP round-trip between two nodes on the same net, including the
    /// connected-socket filter on the client side.
    #[test]
    fn p3_udp_round_trip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let hub = NetHub::new();
            let net = NodeId::nil();
            let client_stack = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "client");
            let server_stack = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "server");
            let engine = engine();
            let mut client_store = store_on(&engine, Some(NetCtx::new(client_stack, hub.clone())));
            let mut server_store = store_on(&engine, Some(NetCtx::new(server_stack, hub.clone())));

            let (bound_tx, bound_rx) = oneshot::channel::<()>();

            let server = server_store.run_concurrent(async move |acc| {
                use wasi::sockets::types::HostUdpSocket as H;
                use wasi::sockets::types::HostUdpSocketWithStore as S;
                let nacc: Nacc = acc.with_getter(|s| SockImpl(s));
                let sock = must(nacc.with(|mut a| H::create(&mut a.get(), IpAddressFamily::Ipv4)));
                must(nacc.with(|mut a| H::bind(&mut a.get(), res(&sock), v4(10, 0, 0, 2, 4242))));
                bound_tx.send(()).expect("signal bound");
                let (data, from) = must(S::receive(&nacc, res(&sock)).await);
                assert_eq!(data, b"ping");
                assert!(matches!(from, IpSocketAddress::Ipv4(p) if p.address == (10, 0, 0, 1)));
                must(S::send(&nacc, res(&sock), b"pong".to_vec(), Some(from)).await);
                wasmtime::error::Ok(())
            });

            let client = client_store.run_concurrent(async move |acc| {
                use wasi::sockets::types::HostUdpSocket as H;
                use wasi::sockets::types::HostUdpSocketWithStore as S;
                let nacc: Nacc = acc.with_getter(|s| SockImpl(s));
                bound_rx.await.expect("server bound");
                let sock = must(nacc.with(|mut a| H::create(&mut a.get(), IpAddressFamily::Ipv4)));
                // `connect` performs the implicit bind and pins the peer.
                must(
                    nacc.with(|mut a| H::connect(&mut a.get(), res(&sock), v4(10, 0, 0, 2, 4242))),
                );
                must(S::send(&nacc, res(&sock), b"ping".to_vec(), None).await);
                let (data, from) = must(S::receive(&nacc, res(&sock)).await);
                assert_eq!(data, b"pong");
                assert!(matches!(from, IpSocketAddress::Ipv4(p) if p.address == (10, 0, 0, 2)));
                wasmtime::error::Ok(())
            });

            let (sr, cr) = tokio::join!(server, client);
            sr.expect("server run_concurrent").expect("server body");
            cr.expect("client run_concurrent").expect("client body");
        });
    }

    /// A v6 dial to a port where the guest listens on a **v4** socket is
    /// refused at the SYN — the listener pool is family-scoped — while the v4
    /// dial to the same port establishes. Before family-scoped listening, the
    /// port-only listeners matched both families and accept surfaced a v6
    /// peer on the v4 socket: wasi-libc's accept() aborts converting that
    /// mismatch to a sockaddr (the netsurf→python `http.server` trap).
    #[test]
    fn p3_v6_dial_to_a_v4_listener_is_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let hub = NetHub::new();
            let net = NodeId::nil();
            let client_stack = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "client");
            let server_stack = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "server");
            let (server_ip, server_ip6) = {
                let g = server_stack.lock().unwrap();
                (g.ip, g.ip6)
            };
            let engine = engine();
            let mut server_store = store_on(&engine, Some(NetCtx::new(server_stack, hub.clone())));

            server_store
                .run_concurrent(async move |acc| {
                    use wasi::sockets::types::HostTcpSocket as H;
                    use wasi::sockets::types::HostTcpSocketWithStore as S;
                    let nacc: Nacc = acc.with_getter(|s| SockImpl(s));
                    let sock =
                        must(nacc.with(|mut a| H::create(&mut a.get(), IpAddressFamily::Ipv4)));
                    // The python http.server case: a v4 wildcard bind.
                    must(
                        nacc.with(|mut a| H::bind(&mut a.get(), res(&sock), v4(0, 0, 0, 0, 8080))),
                    );
                    let _accepts = must(nacc.with(|a| S::listen(a, res(&sock))));

                    // Dial the server's fabric ULA — the v6 route to the port.
                    // The SYN must meet no listener and be answered with RST.
                    let dial = |dst: smoltcp::wire::IpAddress, lport: u16| {
                        let mut g = client_stack.lock().unwrap();
                        let h = g.sockets.add(fabric_tcp_socket());
                        let _gen = g.track(h);
                        let NodeStack { iface, sockets, .. } = &mut *g;
                        sockets
                            .get_mut::<tcp::Socket>(h)
                            .connect(iface.context(), (dst, 8080), lport)
                            .unwrap();
                        h
                    };
                    let h6 = dial(server_ip6.into(), 49500);
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        let state = client_stack
                            .lock()
                            .unwrap()
                            .sockets
                            .get::<tcp::Socket>(h6)
                            .state();
                        assert_ne!(
                            state,
                            tcp::State::Established,
                            "a v6 dial must never land on a v4-bound listener"
                        );
                        if state == tcp::State::Closed {
                            break;
                        }
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the v6 dial was neither refused nor established"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    }

                    // The v4 route to the same port still establishes.
                    let h4 = dial(server_ip.into(), 49501);
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        let state = client_stack
                            .lock()
                            .unwrap()
                            .sockets
                            .get::<tcp::Socket>(h4)
                            .state();
                        if state == tcp::State::Established {
                            break;
                        }
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the v4 dial never established"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    }
                    wasmtime::error::Ok(())
                })
                .await
                .expect("run_concurrent")
                .expect("test body");
        });
    }

    /// A store with no `NetCtx` (the node has no network) gets `access-denied`
    /// from socket creation and can't resolve peer names — never host-OS
    /// sockets. `localhost` still resolves, as in 0.2.
    #[test]
    fn p3_no_network_means_no_sockets() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let engine = engine();
            let mut store = store_on(&engine, None);
            store
                .run_concurrent(async |acc| {
                    use ip_name_lookup::HostWithStore as L;
                    let nacc: Nacc = acc.with_getter(|s| SockImpl(s));
                    let tcp_err = code(nacc.with(|mut a| {
                        wasi::sockets::types::HostTcpSocket::create(
                            &mut a.get(),
                            IpAddressFamily::Ipv4,
                        )
                    }));
                    assert!(matches!(tcp_err, ErrorCode::AccessDenied));
                    let udp_err = code(nacc.with(|mut a| {
                        wasi::sockets::types::HostUdpSocket::create(
                            &mut a.get(),
                            IpAddressFamily::Ipv4,
                        )
                    }));
                    assert!(matches!(udp_err, ErrorCode::AccessDenied));
                    // Peer names don't resolve without a network...
                    assert!(matches!(
                        L::resolve_addresses(&nacc, "some-peer".into()).await?,
                        Err(ip_name_lookup::ErrorCode::NameUnresolvable)
                    ));
                    // ...but localhost always does (loopback needs no fabric).
                    let addrs = L::resolve_addresses(&nacc, "localhost".into())
                        .await?
                        .expect("localhost resolves");
                    assert!(matches!(addrs[0], IpAddress::Ipv4((127, 0, 0, 1))));
                    wasmtime::error::Ok(())
                })
                .await
                .expect("run_concurrent")
                .expect("test body");
        });
    }

    /// Fabric DNS: a peer node's name on the same virtual network resolves to
    /// its fabric addresses, IPv4 (canonical) first — identical to 0.2.
    #[test]
    fn p3_name_lookup_resolves_fabric_peer() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let hub = NetHub::new();
            let net = NodeId::nil();
            let alpha = hub.attach(net, Ipv4Address::new(10, 0, 0, 1), "alpha");
            let _beta = hub.attach(net, Ipv4Address::new(10, 0, 0, 2), "beta");
            let engine = engine();
            let mut store = store_on(&engine, Some(NetCtx::new(alpha, hub.clone())));
            store
                .run_concurrent(async |acc| {
                    use ip_name_lookup::HostWithStore as L;
                    let nacc: Nacc = acc.with_getter(|s| SockImpl(s));
                    let addrs = L::resolve_addresses(&nacc, "beta".into())
                        .await?
                        .expect("peer resolves");
                    assert!(matches!(addrs[0], IpAddress::Ipv4((10, 0, 0, 2))));
                    assert!(addrs
                        .iter()
                        .any(|a| matches!(a, IpAddress::Ipv6((0xfd00, .., 2)))));
                    // A name on no fabric net (and no gateway) stays
                    // unresolvable — the same isolation boundary as 0.2.
                    assert!(matches!(
                        L::resolve_addresses(&nacc, "example.com".into()).await?,
                        Err(ip_name_lookup::ErrorCode::NameUnresolvable)
                    ));
                    wasmtime::error::Ok(())
                })
                .await
                .expect("run_concurrent")
                .expect("test body");
        });
    }
}
