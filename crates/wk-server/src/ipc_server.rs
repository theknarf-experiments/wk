//! The server side of wk's CLI socket — the "docker daemon" of a running
//! workspace.
//!
//! When `wk run` starts a server (windowed or headless), it also starts this
//! listener on a per-workspace Unix socket. A separate `wk` process connects,
//! sends [`ClientMsg`]s, and drives the same server the UI does — reading
//! [`Snapshot`](wk_protocol::ipc::Snapshot)s and issuing [`Command`]s live.
//!
//! The socket path is derived from the workspace file, so the CLI finds it with
//! only `-f`. A local Unix socket is the trust boundary (as with Docker's
//! `/var/run/docker.sock`): the caller already has filesystem access to the
//! user's session, so connections are served with the admin token the listener
//! was started with. The socket is created `0600`.

use std::io::{self, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sha2::Digest;
use wk_protocol::ipc::{read_msg, write_msg, ClientMsg, ServerMsg};

use crate::runtime::ServerHandle;

/// The Unix socket path for a running server on `workspace`. Both the server
/// (to bind) and the CLI (to connect) compute this from the same file, so no
/// discovery is needed. Hashed so a long/nested workspace path stays under the
/// ~104-char socket-path limit.
pub fn socket_path(workspace: &Path) -> PathBuf {
    let abs = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let digest = sha2::Sha256::digest(abs.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    runtime_dir().join(format!("wk-{hex}.sock"))
}

/// Where sockets live: `$XDG_RUNTIME_DIR/wk` if set, else a temp subdir.
fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("wk");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A running socket listener. Drop or [`shutdown`](Self::shutdown) to stop it
/// and remove the socket file.
pub struct IpcServer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl IpcServer {
    /// Bind the workspace's socket and start accepting CLI clients, each served
    /// with `handle`'s authority. A stale socket from a crashed prior run is
    /// replaced.
    pub fn start(handle: ServerHandle, workspace: &Path) -> io::Result<IpcServer> {
        let path = socket_path(workspace);
        let _ = std::fs::remove_file(&path); // clear any stale socket
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("wk-ipc".into())
                .spawn(move || accept_loop(listener, handle, stop))?
        };
        Ok(IpcServer {
            path,
            stop,
            thread: Some(thread),
        })
    }

    /// Where this server is listening (for logging).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop the listener and remove the socket file.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Accept connections until stopped, serving each on its own thread. The
/// listener is non-blocking, so the loop can poll `stop` between clients.
fn accept_loop(listener: UnixListener, handle: ServerHandle, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let handle = handle.clone();
                let stop = stop.clone();
                let _ = thread::Builder::new()
                    .name("wk-ipc-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve_client(stream, handle, stop) {
                            if e.kind() != io::ErrorKind::UnexpectedEof {
                                eprintln!("wk: ipc client error: {e}");
                            }
                        }
                    });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                eprintln!("wk: ipc accept error: {e}");
                break;
            }
        }
    }
}

/// A live terminal attach: the node id (so the UI-detach flag can be cleared)
/// and the pump thread streaming its output to the client.
struct Attach {
    node: wk_protocol::NodeId,
    term: crate::terminal::SharedTermIo,
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
fn serve_client(
    stream: UnixStream,
    handle: ServerHandle,
    _stop: Arc<AtomicBool>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    // Both this loop (acks) and the attach pump (terminal output) write to the
    // socket; share one locked writer so their JSON lines never interleave.
    let writer = Arc::new(std::sync::Mutex::new(stream.try_clone()?));
    let mut reader = BufReader::new(stream);
    let send = |w: &std::sync::Mutex<UnixStream>, m: &ServerMsg| -> io::Result<()> {
        write_msg(&mut *w.lock().unwrap(), m)
    };

    let mut attach: Option<Attach> = None;
    while let Some(msg) = read_msg::<_, ClientMsg>(&mut reader)? {
        match msg {
            ClientMsg::GetSnapshot => send(&writer, &ServerMsg::Snapshot(handle.snapshot()))?,
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
fn spawn_pump(
    term: crate::terminal::SharedTermIo,
    writer: Arc<std::sync::Mutex<UnixStream>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket path is stable per workspace, under the runtime dir, and short
    /// enough for the OS socket-path limit.
    #[test]
    fn socket_path_is_stable_and_short() {
        let a = socket_path(Path::new("example/live-coding.wk"));
        let b = socket_path(Path::new("example/live-coding.wk"));
        assert_eq!(a, b);
        assert_ne!(a, socket_path(Path::new("example/audio.wk")));
        assert!(a.to_string_lossy().len() < 104, "under sun_path limit");
        assert!(a.extension().is_some_and(|e| e == "sock"));
    }
}
