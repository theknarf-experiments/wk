//! What it means to *be* a client of a [`Server`]. A client owns its own loop:
//! it decides how input arrives, how (or whether) to render, and when to stop —
//! then drives the server by applying [`crate::protocol::Command`]s and calling
//! [`Server::tick`]. Single-player runs the [`crate::compositor::WindowClient`]
//! in-process; `--headless` runs [`HeadlessClient`]; future test-runners, MCP
//! bridges, and networked front-ends are just more `impl Client`s.

use crate::server::Server;
use std::time::Duration;

/// A driver over a [`Server`]. `run` takes ownership of the loop and the server,
/// returning when the client decides to exit (window closed, signal, etc.).
/// Boxed-`self` so a caller can pick a client at runtime behind `dyn Client`.
pub trait Client {
    fn run(self: Box<Self>, server: Server) -> Result<(), String>;
}

/// A windowless client: no rendering, no OS input. It keeps the process alive so
/// the guests (which run on their own threads) keep running, ticks the server to
/// maintain network membership, and exits cleanly on Ctrl-C — persisting the
/// workspace on the way out, just like the window client does.
pub struct HeadlessClient;

impl Client for HeadlessClient {
    fn run(self: Box<Self>, mut server: Server) -> Result<(), String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        eprintln!("wk: running headless — press Ctrl-C to stop");
        rt.block_on(async {
            let mut beat = tokio::time::interval(Duration::from_millis(16));
            loop {
                tokio::select! {
                    _ = beat.tick() => server.tick(),
                    _ = tokio::signal::ctrl_c() => break,
                }
            }
        });
        // No window camera to carry back; persist the server's own view.
        let cam = server.camera;
        server.save(cam);
        Ok(())
    }
}
