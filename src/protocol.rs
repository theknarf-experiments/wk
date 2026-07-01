//! The client → server protocol: the vocabulary a client uses to drive a
//! [`crate::server::Server`]. In single-player these are applied in-process via
//! [`crate::server::Server::apply`]; the same enum is what a networked client
//! would serialize over a socket. Positions come *from* the client (it knows its
//! camera) so the server never needs a view.

/// A connection wire, identified by the two node ids it joins (by kind).
#[derive(Clone, Copy, PartialEq)]
pub enum Wire {
    /// A file node (`file_id`) mounted into an app node (`app_id`).
    File(u64, u64),
    /// A MIDI link from source node to destination node.
    Midi(u64, u64),
    /// A wasi:http node served on a HostPort node.
    Serve(u64, u64),
    /// An app node's membership of a Network/Gateway node (app, net).
    Net(u64, u64),
}

/// A mutation a client asks the server to perform.
pub enum Command {
    /// Launch the dependency at index `dep` (in the workspace's list) at `pos`.
    Launch { dep: usize, pos: [f32; 2] },
    /// Create an in-memory shared file node at `pos`.
    AddVirtualFile { pos: [f32; 2] },
    /// Create a disk-backed file node at `pos`.
    AddHostFile { pos: [f32; 2] },
    /// Create a HostPort node at `pos`.
    AddPort { pos: [f32; 2] },
    /// Create a Network node at `pos`.
    AddNetwork { pos: [f32; 2] },
    /// Create a Gateway node at `pos`.
    AddGateway { pos: [f32; 2] },
    /// Remove any node (app/file/port/network) by id.
    RemoveNode { id: u64 },
    /// Move a node to a new canvas position.
    MoveNode { id: u64, pos: [f32; 2] },
    /// Resize a node.
    ResizeNode { id: u64, size: [f32; 2] },
    /// Toggle a connection between two nodes (the kind is inferred from them).
    Connect { a: u64, b: u64 },
    /// Remove a specific connection.
    Disconnect { wire: Wire },
    /// (Re)run an idle/exited app node's guest.
    RunNode { id: u64 },
    /// Set a node's launch args from a whitespace-separated string.
    SetNodeArgs { id: u64, args: String },
    /// Nudge a HostPort's localhost port by `delta`.
    ChangePort { id: u64, delta: i32 },
}
