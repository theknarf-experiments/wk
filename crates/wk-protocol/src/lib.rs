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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

/// The kinds of resource a [`Command`] acts on. Together with an [`Action`] this
/// is the unit of authorization: a token grants `right(resource, action)` pairs
/// (Biscuit facts) and the server checks each command against them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceKind {
    /// A workspace tab.
    Workspace,
    /// A canvas node (app/file/port/network).
    Node,
    /// A connection between two nodes.
    Wire,
    /// The document as a whole (reads, undo).
    Document,
}

impl ResourceKind {
    /// The stable name used in the Biscuit `right(resource, action)` fact.
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Workspace => "workspace",
            ResourceKind::Node => "node",
            ResourceKind::Wire => "wire",
            ResourceKind::Document => "document",
        }
    }

    pub const ALL: [ResourceKind; 4] = [
        ResourceKind::Workspace,
        ResourceKind::Node,
        ResourceKind::Wire,
        ResourceKind::Document,
    ];
}

/// What a [`Command`] does to a resource: CRUD verbs plus the two actions that
/// were never CRUD — `Arrange` (cosmetic layout: move/resize, so a layout-only
/// client can tidy the canvas without being able to reconfigure nodes) and
/// `Run` (start a node's guest).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Create,
    Read,
    Update,
    Delete,
    /// Reposition or resize — cosmetic layout, weaker than `Update`.
    Arrange,
    /// (Re)start a node's guest.
    Run,
}

impl Action {
    /// The stable name used in the Biscuit `right(resource, action)` fact.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Read => "read",
            Action::Update => "update",
            Action::Delete => "delete",
            Action::Arrange => "arrange",
            Action::Run => "run",
        }
    }

    pub const ALL: [Action; 6] = [
        Action::Create,
        Action::Read,
        Action::Update,
        Action::Delete,
        Action::Arrange,
        Action::Run,
    ];
}

/// What kind of node to create (the create payload for [`Resource::Node`]).
pub enum NodeKind {
    /// Launch the dependency at this index in the document's list.
    App { dep: usize },
    /// An in-memory shared file.
    VirtualFile,
    /// A disk-backed file.
    HostFile,
    /// A localhost HostPort.
    Port,
    /// An isolated virtual network.
    Network,
    /// A network whose members get host access.
    Gateway,
}

/// A resource to create.
pub enum Resource {
    /// A node of `kind` at `pos` in workspace `ws`. Positions come *from* the
    /// client (it knows its camera) so the server never needs a view.
    Node {
        kind: NodeKind,
        pos: [f32; 2],
        ws: NodeId,
    },
    /// A connection between two nodes (the kind is inferred from them). No-op if
    /// they are already wired — removal is [`ResourceRef::Wire`] + Delete, never
    /// a side effect of create.
    Wire { a: NodeId, b: NodeId },
    /// A new (empty) workspace with a client-minted id, so the client can switch
    /// its own view to the new tab immediately.
    Workspace { id: NodeId },
}

/// A reference to an existing resource (for deletes).
pub enum ResourceRef {
    Node(NodeId),
    Wire(Wire),
    /// Deleting a workspace removes every node in it. Ignored for the last
    /// workspace (a document always keeps at least one).
    Workspace(NodeId),
}

/// A partial update to a node; only the present fields change.
#[derive(Default)]
pub struct NodePatch {
    /// Move to a new canvas position (requires only `Arrange`).
    pub pos: Option<[f32; 2]>,
    /// Resize (requires only `Arrange`).
    pub size: Option<[f32; 2]>,
    /// Set launch args from a whitespace-separated string (requires `Update`).
    pub args: Option<String>,
    /// Nudge a HostPort's localhost port by this delta (requires `Update`).
    pub port_delta: Option<i32>,
}

/// A mutation a client asks the server to perform: create/update/delete on a
/// resource, plus the non-CRUD actions (run, duplicate, undo).
pub enum Command {
    Create(Resource),
    Update {
        id: NodeId,
        patch: NodePatch,
    },
    Delete(ResourceRef),
    /// (Re)run an idle/exited app node's guest.
    Run(NodeId),
    /// Duplicate a node in place (same workspace, offset position). App nodes
    /// keep their current args and knob settings; wiring is not copied.
    Duplicate(NodeId),
    /// Undo the last undoable mutation.
    Undo,
}

impl Command {
    /// The `right(resource, action)` a client's token must grant for the server
    /// to apply this command.
    pub fn required(&self) -> (ResourceKind, Action) {
        match self {
            Command::Create(Resource::Node { .. }) => (ResourceKind::Node, Action::Create),
            Command::Create(Resource::Wire { .. }) => (ResourceKind::Wire, Action::Create),
            Command::Create(Resource::Workspace { .. }) => {
                (ResourceKind::Workspace, Action::Create)
            }
            // A patch touching args/port reconfigures the node; pos/size alone
            // is cosmetic layout.
            Command::Update { patch, .. } => {
                if patch.args.is_some() || patch.port_delta.is_some() {
                    (ResourceKind::Node, Action::Update)
                } else {
                    (ResourceKind::Node, Action::Arrange)
                }
            }
            Command::Delete(ResourceRef::Node(_)) => (ResourceKind::Node, Action::Delete),
            Command::Delete(ResourceRef::Wire(_)) => (ResourceKind::Wire, Action::Delete),
            Command::Delete(ResourceRef::Workspace(_)) => (ResourceKind::Workspace, Action::Delete),
            Command::Run(_) => (ResourceKind::Node, Action::Run),
            Command::Duplicate(_) => (ResourceKind::Node, Action::Create),
            // Undo can restore or remove anything it previously recorded, so it
            // needs document-wide write authority.
            Command::Undo => (ResourceKind::Document, Action::Update),
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
