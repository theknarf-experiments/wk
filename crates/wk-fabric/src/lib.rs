//! wk's userspace **network fabric**, isolated from the server and the wasm
//! host so it can be built, tested, and reasoned about on its own (and so the
//! heavy p2p dependencies don't sit on the server crate's rebuild path).
//!
//! The fabric owns the network the way wk's vfs owns the filesystem: each
//! networked node gets a virtual NIC + its own smoltcp stack on a hub that
//! routes raw IP packets between same-network peers ([`netstack`]). Because it
//! moves *packets*, everything above is composable plumbing:
//!
//! - [`netstack::TrunkPort`] — a tap for frames with no local destination,
//!   the primitive uplinks and future middleboxes (VPN/proxy) build on;
//! - [`portfwd`] — publish a fabric TCP service on a localhost port;
//! - [`uplink`] — extend a network to a remote fabric over iroh p2p QUIC;
//! - [`veilid`] — the same, over Veilid's onion-routed network.
//!
//! The wasi:sockets binding that terminates guest sockets in these stacks
//! lives with the wasm host (wk-server), not here; this crate's boundary is
//! [`netstack::SharedStack`].

pub mod netstack;
pub mod portfwd;
pub mod uplink;
pub mod veilid;
