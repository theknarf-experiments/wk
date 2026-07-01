//! The wk **local UI client**: the single-player front-end. It renders the
//! workspace in a wgpu/winit window, turns OS input into
//! [`wk_protocol::Command`]s, and drives a [`wk_server::server::Server`] to
//! completion. The windowless [`HeadlessClient`] lives here too — it's the same
//! client contract without any rendering, used by `wk run --headless`.
//!
//! Both implement [`wk_protocol::Client`]; the `wk` binary picks one and hands
//! it the server. Everything view/input (camera, selection, palette, drag,
//! terminals, textures) lives in this crate; the server never sees it.

mod arrows;
mod client;
mod compositor;
mod host_shell;
mod render2d;
mod text;

pub use client::HeadlessClient;
pub use compositor::WindowClient;
