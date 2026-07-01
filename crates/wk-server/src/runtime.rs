//! Running the [`Server`] as an independent service. The server owns its own
//! thread and loop — it drains client commands and ticks on its own schedule,
//! regardless of whether any client is attached. Clients talk to it only through
//! a [`ServerHandle`]: they send [`Command`]s and read [`View`] snapshots. The
//! handle is cloneable, so any number of clients can attach at once; "headless"
//! is simply spawning the runtime and attaching none.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wk_protocol::Command;

use crate::server::{Server, View};
use crate::workspace::Workspace;

/// How often the server loop drains commands and ticks (~60 Hz).
const STEP: Duration = Duration::from_millis(16);

/// A client's connection to a running server. Cloneable and `Send`, so every
/// attached client (local UI, MCP bridge, network peer) holds its own. Writes go
/// out as [`Command`]s; reads come back as [`View`] snapshots. A client never
/// touches the [`Server`] directly.
#[derive(Clone)]
pub struct ServerHandle {
    cmds: Sender<Command>,
    server: Arc<Mutex<Server>>,
}

impl ServerHandle {
    /// Queue a command for the server to apply on its next step. Never blocks on
    /// the server; ordering is preserved per sender. Dropping the server makes
    /// this a no-op.
    pub fn send(&self, cmd: Command) {
        let _ = self.cmds.send(cmd);
    }

    /// A fresh render snapshot of the current server state.
    pub fn view(&self) -> View {
        self.server.lock().unwrap().view()
    }
}

/// Owns the server thread. Hand out [`handle`](Self::handle)s to attach clients;
/// call [`shutdown`](Self::shutdown) (or [`block_until_ctrl_c`](Self::block_until_ctrl_c))
/// to stop the loop, which persists the workspace on its way out.
pub struct ServerRuntime {
    handle: ServerHandle,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ServerRuntime {
    /// Instantiate the workspace and start the server loop on its own thread.
    pub fn spawn(ws: &Workspace, path: std::path::PathBuf) -> Result<Self, String> {
        let server = Server::new(ws, path)?;
        let server = Arc::new(Mutex::new(server));
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = ServerHandle {
            cmds: tx,
            server: server.clone(),
        };
        let thread = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("wk-server".into())
                .spawn(move || serve(server, rx, stop))
                .map_err(|e| e.to_string())?
        };
        Ok(ServerRuntime {
            handle,
            stop,
            thread: Some(thread),
        })
    }

    /// A connection a client attaches through. Clone freely for multiple clients.
    pub fn handle(&self) -> ServerHandle {
        self.handle.clone()
    }

    /// Stop the loop and join the thread; the loop saves the workspace as it
    /// exits.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Headless run: keep the server going with no client attached until Ctrl-C,
    /// then shut down (persisting the workspace).
    pub fn block_until_ctrl_c(self) {
        eprintln!("wk: server running headless (no client attached) — press Ctrl-C to stop");
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("wk: failed to wait for Ctrl-C: {e}");
                self.shutdown();
                return;
            }
        };
        rt.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
        });
        self.shutdown();
    }
}

/// The server loop: drain queued commands, advance the runtime, tick, sleep.
/// Runs until `stop` is set, then persists the workspace.
fn serve(server: Arc<Mutex<Server>>, rx: Receiver<Command>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        {
            let mut s = server.lock().unwrap();
            while let Ok(cmd) = rx.try_recv() {
                s.apply(cmd);
            }
            // Advance the epoch so any runaway guest re-checks its kill switch,
            // then reconcile wiring that was pending on a still-loading node.
            s.host.tick_epoch();
            s.tick();
        }
        thread::sleep(STEP);
    }
    let s = server.lock().unwrap();
    let cam = s.camera;
    s.save(cam);
}
