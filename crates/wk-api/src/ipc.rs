//! The **local IPC transport**: wk's CLI socket, the "docker daemon" of a
//! running workspace.
//!
//! When `wk run` starts a server (windowed or headless), it also starts this
//! listener on a per-workspace Unix socket. A separate `wk` process connects,
//! sends [`ClientMsg`](wk_protocol::ipc::ClientMsg)s, and drives the same
//! server the UI does.
//!
//! This module is *only* the transport: binding the socket, accepting
//! connections, and deciding their authority. The trust decision here is
//! Docker's (`/var/run/docker.sock`): a local Unix socket (created `0600`) is
//! the boundary — the caller already has filesystem access to the user's
//! session — so every connection is served with the admin-token handle the
//! listener was started with. The message loop itself is the transport-neutral
//! [`serve_client`](crate::serve_client); a networked transport supplies its
//! own (stricter) authority decision and reuses that loop.

use std::io::{self, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sha2::Digest;
use wk_server::runtime::ServerHandle;

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

/// Accept connections until stopped, serving each on its own thread through
/// the transport-neutral [`serve_client`](crate::serve_client). The listener
/// is non-blocking, so the loop can poll `stop` between clients.
fn accept_loop(listener: UnixListener, handle: ServerHandle, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let handle = handle.clone();
                let _ = thread::Builder::new()
                    .name("wk-ipc-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve_stream(stream, handle) {
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

/// Split one accepted socket into the reader/shared-writer halves the message
/// loop expects (both the loop's replies and the attach pump write, so the
/// writer is behind one lock — their JSON lines never interleave).
fn serve_stream(stream: UnixStream, handle: ServerHandle) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    let writer = Arc::new(std::sync::Mutex::new(stream.try_clone()?));
    let reader = BufReader::new(stream);
    crate::serve_client(handle, reader, writer)
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
