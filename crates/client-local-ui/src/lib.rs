//! The wk **local UI client**: the single-player front-end. It renders the
//! workspace in a wgpu/winit window, turns OS input into [`wk_protocol::Command`]s
//! it sends to the server, and reads back render snapshots — all over a
//! [`wk_server::runtime::ServerHandle`]. The server runs independently on its own
//! thread; this is just one client attached to it (a headless run attaches none).
//!
//! [`WindowClient`] implements [`wk_protocol::Client`]. Everything view/input
//! (camera, selection, palette, drag, terminals, textures) lives in this crate;
//! the server never sees it.

mod arrows;
mod compositor;
mod host_shell;
mod render2d;
mod text;

pub use compositor::WindowClient;
