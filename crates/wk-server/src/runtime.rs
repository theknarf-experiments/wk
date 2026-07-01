//! Running the [`Server`] as an independent service. The server owns its own
//! thread and loop — it drains client commands and ticks on its own schedule,
//! regardless of whether any client is attached. Clients talk to it only through
//! a [`ServerHandle`]: they send [`Command`]s (each carrying the bearer's token)
//! and read [`View`] snapshots. The handle is cloneable, so any number of clients
//! can attach at once; "headless" is simply spawning the runtime and attaching
//! none.
//!
//! The server verifies every command against a [`PublicKey`] it was given at
//! spawn — a copy of the token service's key. It never mints tokens.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use biscuit_auth::PublicKey;
use wk_protocol::Command;

use crate::auth;
use crate::server::{Server, View};
use crate::workspace::Workspace;

/// How often the server loop drains commands and ticks (~60 Hz).
const STEP: Duration = Duration::from_millis(16);

/// A message on the command channel: the bearer's token and the command it
/// authorizes. The token travels with every command so the server can verify it
/// independently — exactly as a networked client would send it.
type Envelope = (Vec<u8>, Command);

/// A client's connection to a running server. Cloneable and `Send`, so every
/// attached client (local UI, MCP bridge, network peer) holds its own. A client
/// bears a token (via [`with_token`](Self::with_token)) and presents it with
/// every command; reads come back as [`View`] snapshots. A client never touches
/// the [`Server`] directly.
#[derive(Clone)]
pub struct ServerHandle {
    cmds: Sender<Envelope>,
    server: Arc<Mutex<Server>>,
    /// The bearer token presented with each command. Empty until the client is
    /// handed one; an empty/absent token authorizes nothing.
    token: Arc<Vec<u8>>,
}

impl ServerHandle {
    /// Attach a bearer token to this connection. The client presents it with
    /// every command it sends; the server verifies + authorizes each one.
    pub fn with_token(mut self, token: Vec<u8>) -> Self {
        self.token = Arc::new(token);
        self
    }

    /// Queue a command (with this connection's token) for the server to apply on
    /// its next step, if the token authorizes it. Never blocks on the server;
    /// dropping the server makes this a no-op.
    pub fn send(&self, cmd: Command) {
        let _ = self.cmds.send((self.token.as_ref().clone(), cmd));
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
    /// `public_key` is a copy of the token service's key, used to verify the
    /// token presented with each command.
    pub fn spawn(
        ws: &Workspace,
        path: std::path::PathBuf,
        public_key: PublicKey,
    ) -> Result<Self, String> {
        let server = Server::new(ws, path)?;
        let server = Arc::new(Mutex::new(server));
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = ServerHandle {
            cmds: tx,
            server: server.clone(),
            token: Arc::new(Vec::new()),
        };
        let thread = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("wk-server".into())
                .spawn(move || serve(server, rx, stop, public_key))
                .map_err(|e| e.to_string())?
        };
        Ok(ServerRuntime {
            handle,
            stop,
            thread: Some(thread),
        })
    }

    /// A connection a client attaches through. Clone (and give a token via
    /// [`ServerHandle::with_token`]) for each client.
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

/// The server loop: drain queued commands (authorizing each), advance the
/// runtime, tick, sleep. Runs until `stop` is set, then persists the workspace.
fn serve(
    server: Arc<Mutex<Server>>,
    rx: Receiver<Envelope>,
    stop: Arc<AtomicBool>,
    public_key: PublicKey,
) {
    while !stop.load(Ordering::Relaxed) {
        {
            let mut s = server.lock().unwrap();
            while let Ok((token, cmd)) = rx.try_recv() {
                if auth::authorize(public_key, &token, cmd.operation()) {
                    s.apply(cmd);
                } else {
                    eprintln!("wk: rejected unauthorized command ({:?})", cmd.operation());
                }
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
