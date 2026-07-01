//! The contract between a wk **client** and **server**, isolated in its own
//! crate so the seam is explicit and free of any implementation detail.
//!
//! - [`Command`] (+ [`Wire`]) is the client → server vocabulary: the set of
//!   mutations a client may ask the server to perform. In single-player these
//!   are applied in-process; the same enum is what a networked client would
//!   serialize over a socket.
//! - [`Client`] is the front-end contract: a client owns its own loop (how input
//!   arrives, whether to render, when to stop) and attaches to a server through a
//!   connection handle, but never owns or drives the server itself.
//!
//! This crate deliberately has no knowledge of the server's internals: it never
//! names the concrete `Server`, only the messages that cross the boundary and
//! the trait a front-end plugs into. That keeps it trivially reusable by future
//! test-runners, MCP bridges, and networked front-ends.

mod node_id;
pub use node_id::NodeId;

/// A connection wire, identified by the two node ids it joins (by kind).
#[derive(Clone, Copy, PartialEq)]
pub enum Wire {
    /// A file node (`file_id`) mounted into an app node (`app_id`).
    File(NodeId, NodeId),
    /// A MIDI link from source node to destination node.
    Midi(NodeId, NodeId),
    /// A wasi:http node served on a HostPort node.
    Serve(NodeId, NodeId),
    /// An app node's membership of a Network/Gateway node (app, net).
    Net(NodeId, NodeId),
}

/// The capability a [`Command`] requires. A client's token grants some set of
/// these; the server authorizes each command against the token that carried it.
/// Kept crypto-free here so both sides agree on the taxonomy; the server maps
/// each to a Biscuit `right(..)` fact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Operation {
    /// Create a node of any kind (launch an app, add a file/port/network).
    Create,
    /// Remove a node.
    Remove,
    /// Add or remove a connection between nodes.
    Wire,
    /// Run/configure a node (run, set args, change a port).
    Control,
    /// Reposition or resize a node. Cosmetic layout.
    Arrange,
}

impl Operation {
    /// The stable name used in the Biscuit `right(..)` fact and policy.
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Create => "create",
            Operation::Remove => "remove",
            Operation::Wire => "wire",
            Operation::Control => "control",
            Operation::Arrange => "arrange",
        }
    }

    /// Every operation, for minting a full-authority token.
    pub const ALL: [Operation; 5] = [
        Operation::Create,
        Operation::Remove,
        Operation::Wire,
        Operation::Control,
        Operation::Arrange,
    ];
}

/// A mutation a client asks the server to perform. Positions come *from* the
/// client (it knows its camera) so the server never needs a view.
pub enum Command {
    /// Launch the dependency at index `dep` at `pos` in workspace `ws`.
    Launch {
        dep: usize,
        pos: [f32; 2],
        ws: NodeId,
    },
    /// Create an in-memory shared file node at `pos` in workspace `ws`.
    AddVirtualFile { pos: [f32; 2], ws: NodeId },
    /// Create a disk-backed file node at `pos` in workspace `ws`.
    AddHostFile { pos: [f32; 2], ws: NodeId },
    /// Create a HostPort node at `pos` in workspace `ws`.
    AddPort { pos: [f32; 2], ws: NodeId },
    /// Create a Network node at `pos` in workspace `ws`.
    AddNetwork { pos: [f32; 2], ws: NodeId },
    /// Create a Gateway node at `pos` in workspace `ws`.
    AddGateway { pos: [f32; 2], ws: NodeId },
    /// Create a new (empty) workspace with the given client-minted id. The client
    /// mints the id so it can switch its own view to the new tab immediately.
    AddWorkspace { id: NodeId },
    /// Delete a workspace and every node in it. Ignored for the last workspace
    /// (a document always keeps at least one).
    RemoveWorkspace { id: NodeId },
    /// Remove any node (app/file/port/network) by id.
    RemoveNode { id: NodeId },
    /// Move a node to a new canvas position.
    MoveNode { id: NodeId, pos: [f32; 2] },
    /// Resize a node.
    ResizeNode { id: NodeId, size: [f32; 2] },
    /// Toggle a connection between two nodes (the kind is inferred from them).
    Connect { a: NodeId, b: NodeId },
    /// Remove a specific connection.
    Disconnect { wire: Wire },
    /// (Re)run an idle/exited app node's guest.
    RunNode { id: NodeId },
    /// Set a node's launch args from a whitespace-separated string.
    SetNodeArgs { id: NodeId, args: String },
    /// Nudge a HostPort's localhost port by `delta`.
    ChangePort { id: NodeId, delta: i32 },
}

impl Command {
    /// The capability a client must hold for the server to apply this command.
    pub fn operation(&self) -> Operation {
        match self {
            Command::Launch { .. }
            | Command::AddVirtualFile { .. }
            | Command::AddHostFile { .. }
            | Command::AddPort { .. }
            | Command::AddNetwork { .. }
            | Command::AddGateway { .. }
            | Command::AddWorkspace { .. } => Operation::Create,
            Command::RemoveNode { .. } | Command::RemoveWorkspace { .. } => Operation::Remove,
            Command::Connect { .. } | Command::Disconnect { .. } => Operation::Wire,
            Command::RunNode { .. } | Command::SetNodeArgs { .. } | Command::ChangePort { .. } => {
                Operation::Control
            }
            Command::MoveNode { .. } | Command::ResizeNode { .. } => Operation::Arrange,
        }
    }
}

/// A client attached to a running server through a connection `C`. `run` owns
/// the client's own loop and returns when it decides to detach (window closed,
/// signal, peer disconnect, etc.).
///
/// The server runs independently of any client: `C` is a *connection handle*, not
/// the server itself — a client sends [`Command`]s over it and reads state
/// through it, but never owns or drives the server. The handle is cloneable, so
/// any number of clients (a local UI, an MCP bridge, networked peers) can attach
/// to the same server at once. "Headless" is simply no client attached.
///
/// The trait is generic over the handle type rather than naming it, so this crate
/// stays free of the server's internals. Boxed-`self` so a caller can pick a
/// client at runtime behind `dyn Client<C>`.
pub trait Client<C> {
    fn run(self: Box<Self>, conn: C) -> Result<(), String>;
}
