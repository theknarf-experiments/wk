//! The websocket leg: a tungstenite client over a std TcpStream, which on
//! wasip2 rides `wasi:sockets` — and in wk, the fabric. `ws://subduction:8080`
//! resolves by fabric DNS to a HostService bridge.
//!
//! [`WsTransport`] is the subduction seam: the three-method `Transport`
//! (frame-oriented send/recv/disconnect) plus `Handshake` for the initial
//! challenge/response, both over one nonblocking socket. Futures here never
//! park — on `WouldBlock` they self-wake and return `Pending`, and the
//! busy-poll executor (see `rt`) re-polls them next pump.

use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use futures::future::BoxFuture;
use tungstenite::{Message, WebSocket};

pub type Conn = WebSocket<TcpStream>;

/// Dial `ws://host:port[/path]` and complete the RFC6455 handshake. The
/// stream is left in *nonblocking* mode: the caller pumps it cooperatively
/// between filesystem requests, and a blocking read would stall serving.
pub fn connect(url: &str) -> Result<Conn, String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| format!("unsupported url (ws:// only): {url}"))?;
    let hostport = rest.split('/').next().unwrap_or(rest);
    let target = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    let stream = TcpStream::connect(&target).map_err(|e| format!("tcp {target}: {e}"))?;
    let (ws, resp) =
        tungstenite::client(url, stream).map_err(|e| format!("handshake {url}: {e}"))?;
    if resp.status().as_u16() != 101 {
        return Err(format!("handshake {url}: HTTP {}", resp.status()));
    }
    ws.get_ref()
        .set_nonblocking(true)
        .map_err(|e| format!("nonblocking: {e}"))?;
    Ok(ws)
}

/// A websocket error, stringly: the engine only needs `Error` + `Display`,
/// and every failure here is terminal for the connection anyway.
#[derive(Debug)]
pub struct WsError(pub String);

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "websocket: {}", self.0)
    }
}
impl std::error::Error for WsError {}

fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
}

/// The shared connection subduction talks through. Clone = same socket;
/// equality is identity, which is what the engine's connection set needs.
#[derive(Clone)]
pub struct WsTransport(Arc<Mutex<Conn>>);

impl WsTransport {
    pub fn new(conn: Conn) -> Self {
        WsTransport(Arc::new(Mutex::new(conn)))
    }

    /// One send step: queue the frame if still unqueued, then flush. Returns
    /// `Ready` when fully flushed (or failed), `Pending` on `WouldBlock`.
    fn poll_send(&self, msg: &mut Option<Message>) -> Poll<Result<(), WsError>> {
        let mut ws = self.0.lock().unwrap();
        if let Some(m) = msg.take() {
            match ws.write(m) {
                Ok(()) => {}
                // Out-buffer full: put it back, flush below, retry next poll.
                Err(tungstenite::Error::WriteBufferFull(m)) => *msg = Some(*m),
                Err(e) => return Poll::Ready(Err(WsError(e.to_string()))),
            }
        }
        match ws.flush() {
            Ok(()) if msg.is_none() => Poll::Ready(Ok(())),
            Ok(()) => Poll::Pending,
            Err(e) if would_block(&e) => Poll::Pending,
            Err(e) => Poll::Ready(Err(WsError(e.to_string()))),
        }
    }

    /// One recv step: the next *binary* frame. Control frames are handled by
    /// tungstenite internally (pongs queue on read); text frames are not part
    /// of the subduction protocol and are skipped.
    fn poll_recv(&self) -> Poll<Result<Vec<u8>, WsError>> {
        let mut ws = self.0.lock().unwrap();
        loop {
            match ws.read() {
                Ok(Message::Binary(b)) => return Poll::Ready(Ok(b.into())),
                Ok(Message::Close(c)) => {
                    return Poll::Ready(Err(WsError(format!("closed: {c:?}"))))
                }
                Ok(_) => continue, // text/ping/pong: not protocol frames
                Err(e) if would_block(&e) => return Poll::Pending,
                Err(e) => return Poll::Ready(Err(WsError(e.to_string()))),
            }
        }
    }
}

impl PartialEq for WsTransport {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for WsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WsTransport({:p})", Arc::as_ptr(&self.0))
    }
}

impl subduction_core::transport::Transport<future_form::Sendable> for WsTransport {
    type SendError = WsError;
    type RecvError = WsError;
    type DisconnectionError = WsError;

    fn send_bytes(&self, bytes: &[u8]) -> BoxFuture<'_, Result<(), WsError>> {
        let mut msg = Some(Message::Binary(bytes.to_vec().into()));
        Box::pin(futures::future::poll_fn(move |cx| {
            let r = self.poll_send(&mut msg);
            if r.is_pending() {
                cx.waker().wake_by_ref();
            }
            r
        }))
    }

    fn recv_bytes(&self) -> BoxFuture<'_, Result<Vec<u8>, WsError>> {
        Box::pin(futures::future::poll_fn(move |cx| {
            let r = self.poll_recv();
            if r.is_pending() {
                cx.waker().wake_by_ref();
            }
            r
        }))
    }

    fn disconnect(&self) -> BoxFuture<'_, Result<(), WsError>> {
        let _ = self.0.lock().unwrap().close(None);
        Box::pin(std::future::ready(Ok(())))
    }
}

impl subduction_core::handshake::Handshake<future_form::Sendable> for WsTransport {
    type Error = WsError;

    fn send(&mut self, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), WsError>> {
        let mut msg = Some(Message::Binary(bytes.into()));
        Box::pin(futures::future::poll_fn(move |cx| {
            let r = self.poll_send(&mut msg);
            if r.is_pending() {
                cx.waker().wake_by_ref();
            }
            r
        }))
    }

    fn recv(&mut self) -> BoxFuture<'_, Result<Vec<u8>, WsError>> {
        Box::pin(futures::future::poll_fn(move |cx| {
            let r = self.poll_recv();
            if r.is_pending() {
                cx.waker().wake_by_ref();
            }
            r
        }))
    }
}
