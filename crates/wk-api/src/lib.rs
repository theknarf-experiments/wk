//! The wk **client API service**, transport-agnostic: [`serve_client`] speaks
//! the [`wk_protocol::ipc`] wire messages over any byte stream, driving a
//! running server through a [`ServerHandle`].
//!
//! A transport's job is only to produce an authenticated connection: accept a
//! stream, decide which bearer token the connection holds, attach it to a
//! handle ([`ServerHandle::with_token`]), and pass the stream halves here. The
//! local Unix socket ([`ipc`]) is the first transport — its trust decision is
//! "local = admin". A networked transport makes a different decision (a token
//! presented and verified during its handshake) and reuses this loop
//! unchanged; all actual authorization happens inside the handle/server
//! against that token, never in the transport.

use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wk_protocol::ipc::{read_msg, write_msg, ClientMsg, ServerMsg};
use wk_server::runtime::ServerHandle;

pub mod ipc;

/// A live terminal attach: the node id (so the UI-detach flag can be cleared)
/// and the pump thread streaming its output to the client.
struct Attach {
    node: wk_protocol::NodeId,
    term: wk_server::terminal::SharedTermIo,
    stop: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

impl Attach {
    /// Stop the pump and clear the server-side attach flag.
    fn end(mut self, handle: &ServerHandle) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.pump.take() {
            let _ = t.join();
        }
        handle.set_attached(self.node, false);
    }
}

/// Serve one connected client: read framed [`ClientMsg`]s and reply with
/// [`ServerMsg`]s until the client disconnects. While attached to a node, a
/// pump thread streams that node's terminal output; this loop feeds its input.
///
/// `handle` carries the connection's authority (its bearer token) — every
/// read and command is authorized against it by the server. The `writer` is
/// shared behind a lock because the attach pump writes concurrently with this
/// loop's replies; the transport hands both halves in.
pub fn serve_client<R, W>(
    handle: ServerHandle,
    mut reader: R,
    writer: Arc<Mutex<W>>,
) -> io::Result<()>
where
    R: BufRead,
    W: Write + Send + 'static,
{
    let send =
        |w: &Mutex<W>, m: &ServerMsg| -> io::Result<()> { write_msg(&mut *w.lock().unwrap(), m) };

    let mut attach: Option<Attach> = None;
    while let Some(msg) = read_msg::<_, ClientMsg>(&mut reader)? {
        match msg {
            ClientMsg::GetSnapshot => match handle.snapshot() {
                Ok(snap) => send(&writer, &ServerMsg::Snapshot(snap))?,
                Err(e) => send(&writer, &ServerMsg::Error(e))?,
            },
            ClientMsg::Command(cmd) => {
                handle.send(cmd);
                send(&writer, &ServerMsg::Ok)?;
            }
            ClientMsg::Attach { node } => {
                if let Some(a) = attach.take() {
                    a.end(&handle);
                }
                match handle.term_io(node) {
                    Some(term) if handle.set_attached(node, true) => {
                        let (cols, rows) = term.size();
                        send(&writer, &ServerMsg::Attached { cols, rows })?;
                        let stop = Arc::new(AtomicBool::new(false));
                        let pump = spawn_pump(term.clone(), writer.clone(), stop.clone());
                        attach = Some(Attach {
                            node,
                            term,
                            stop,
                            pump: Some(pump),
                        });
                    }
                    Some(_) => {
                        handle.set_attached(node, false);
                        send(&writer, &ServerMsg::Error("node is not a terminal".into()))?;
                    }
                    None => send(&writer, &ServerMsg::Error("no such node".into()))?,
                }
            }
            ClientMsg::Input(bytes) => {
                if let Some(a) = &attach {
                    a.term.feed_in(&bytes);
                }
            }
            ClientMsg::Resize { cols, rows } => {
                if let Some(a) = &attach {
                    a.term.set_size(cols, rows);
                }
            }
            ClientMsg::Detach => {
                if let Some(a) = attach.take() {
                    a.end(&handle);
                }
                send(&writer, &ServerMsg::Detached)?;
            }
            ClientMsg::Logs { node, follow } => {
                let Some(term) = handle.term_io(node) else {
                    send(&writer, &ServerMsg::Error("no such node".into()))?;
                    continue;
                };
                // Send the current scrollback, then either finish or follow.
                let (bytes, mut cursor) = term.log_read(0);
                if !bytes.is_empty() {
                    send(&writer, &ServerMsg::LogChunk(bytes))?;
                }
                if !follow {
                    send(&writer, &ServerMsg::LogEnd)?;
                    continue;
                }
                // Follow: poll for new output (non-destructive) until disconnect.
                loop {
                    let (chunk, next) = term.log_read(cursor);
                    if !chunk.is_empty() {
                        if send(&writer, &ServerMsg::LogChunk(chunk)).is_err() {
                            break;
                        }
                        cursor = next;
                    } else if term.is_closed() {
                        let _ = send(&writer, &ServerMsg::LogEnd);
                        break;
                    } else {
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
    }
    // Client disconnected — release any attach so the UI reclaims the node.
    if let Some(a) = attach.take() {
        a.end(&handle);
    }
    Ok(())
}

/// Stream a node's terminal output to the client until stopped or the node
/// exits. Drains `term` and writes [`ServerMsg::Term`]; on node exit sends
/// [`ServerMsg::Detached`] so the client returns to its shell.
fn spawn_pump<W: Write + Send + 'static>(
    term: wk_server::terminal::SharedTermIo,
    writer: Arc<Mutex<W>>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let out = term.drain_out();
            if !out.is_empty() {
                if write_msg(&mut *writer.lock().unwrap(), &ServerMsg::Term(out)).is_err() {
                    break;
                }
            } else if term.is_closed() {
                let _ = write_msg(&mut *writer.lock().unwrap(), &ServerMsg::Detached);
                break;
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
    })
}
