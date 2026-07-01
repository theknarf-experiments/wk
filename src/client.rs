//! The concrete clients wk ships. The [`Client`](wk_protocol::Client) contract
//! itself lives in the `wk-protocol` crate; here we implement it. Single-player
//! runs the [`crate::compositor::WindowClient`] in-process; `--headless` runs
//! [`HeadlessClient`]; future test-runners, MCP bridges, and networked
//! front-ends are just more `impl Client`s.

use crate::server::Server;
use std::time::Duration;
use wk_protocol::Client;

/// A windowless client: no rendering, no OS input. It keeps the process alive so
/// the guests (which run on their own threads) keep running, ticks the server to
/// maintain network membership, and exits cleanly on Ctrl-C — persisting the
/// workspace on the way out, just like the window client does.
pub struct HeadlessClient;

impl Client<Server> for HeadlessClient {
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
