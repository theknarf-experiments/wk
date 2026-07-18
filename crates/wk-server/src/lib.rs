//! The wk **backend**: the authoritative half of a running workspace, isolated
//! from any front-end. [`server::Server`] owns the document and the runtime;
//! everything else here is what it runs on — the wasm host ([`plugin`]), the
//! per-instance virtual filesystem ([`vfs`]), the userspace network fabric
//! ([`netstack`]/[`sockets`]), audio, MIDI, HTTP serving, workspace persistence,
//! and the terminal/line-discipline that guests write to.
//!
//! Clients drive this crate through the `wk-protocol` contract; they never reach
//! past [`server::Server`]'s public surface into these internals.

pub mod audio;
pub mod auth;
pub mod http;
pub mod images;
pub mod midi;
pub mod oci;
pub mod options;
pub mod plugin;
pub mod runtime;
pub mod server;
pub mod sockets;
pub mod terminal;
pub mod tty;
pub mod wiring;

// The virtual filesystem (and its immutable layer engine) lives in the
// wk-vfs crate; re-exported here so `crate::vfs`/`crate::layers` paths (and
// downstream `wk_server::vfs` users) stay stable.
pub use wk_vfs as vfs;
pub use wk_vfs::layers;
pub mod workspace;
