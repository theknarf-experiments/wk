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

pub mod ipc;

use serde::{Deserialize, Serialize};

/// A connection wire, identified by the two node ids it joins (by kind).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Wire {
    /// A volume node (`volume_id`) bind-mounted into an app node (`app_id`).
    Bind(NodeId, NodeId),
    /// A MIDI link from source node to destination node.
    Midi(NodeId, NodeId),
    /// A wasi:http node served on a HostPort node.
    Serve(NodeId, NodeId),
    /// An app node's membership of a Network/Gateway node (app, net).
    Net(NodeId, NodeId),
    /// An app node's grant from a Screen Capture node (app, capture) — the app
    /// may read captured frames while wired.
    Capture(NodeId, NodeId),
    /// An app node's grant from an Api node (app, api) — the app may drive
    /// wk's client API over its virtual network while wired.
    Api(NodeId, NodeId),
}

/// The TCP port the wk API listens on inside a node's virtual network, when
/// the node is wired to an Api node. Fabric DNS resolves the endpoint as
/// `api`, so a guest dials `api:1337` and speaks the [`ipc`] wire protocol
/// (newline-delimited JSON), presenting nothing — the connection already
/// bears the node's own capability token.
pub const API_PORT: u16 = 1337;

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
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum NodeKind {
    /// Launch the dependency at this index in the document's list.
    App { dep: usize },
    /// An in-memory named volume (shared across the apps it binds into).
    Volume,
    /// A bind mount backed by a real host path (a file or a folder).
    BindMount,
    /// A localhost HostPort.
    Port,
    /// An isolated virtual network.
    Network,
    /// A network whose members get host access.
    Gateway,
    /// An iroh uplink: extends a Network to a remote fabric over p2p QUIC.
    Iroh,
    /// A Veilid uplink: extends a Network to a remote fabric over Veilid's
    /// onion-routed p2p network.
    Veilid,
    /// A yellow sticky note — a purely visual annotation, wired to nothing.
    Note,
    /// A Screen Capture node: a capability source granting wired apps access
    /// to captured frames (the host does the capturing).
    Capture,
    /// The wk client API as a node: apps wired to it can drive wk over their
    /// virtual network.
    Api,
    /// A hardware MIDI input node: the host opens a physical MIDI input device
    /// and routes its messages to the app nodes it's wired to (a MIDI source,
    /// like the piano plugin).
    MidiIn,
}

/// A resource to create.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Resource {
    /// A node of `kind` at `pos` in workspace `ws`. Positions come *from* the
    /// client (it knows its camera) so the server never needs a view.
    Node {
        kind: NodeKind,
        pos: [f32; 2],
        ws: NodeId,
    },
    /// A BindMount node already pointed at a host path (a file or a folder) —
    /// what dropping a file from the OS onto the canvas creates, in one
    /// undoable step instead of create-then-point.
    HostMount {
        path: String,
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
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ResourceRef {
    Node(NodeId),
    Wire(Wire),
    /// Deleting a workspace removes every node in it. Ignored for the last
    /// workspace (a document always keeps at least one).
    Workspace(NodeId),
}

/// A partial update to a node; only the present fields change.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct NodePatch {
    /// Move to a new canvas position (requires only `Arrange`).
    pub pos: Option<[f32; 2]>,
    /// Place at a free 3D pose in the world — `[x, y, z, yaw]` in world units
    /// (requires only `Arrange`). Nodes without one sit on the default layout
    /// cylinder derived from their canvas position.
    pub pos3d: Option<[f32; 4]>,
    /// Resize (requires only `Arrange`).
    pub size: Option<[f32; 2]>,
    /// Set launch args from a whitespace-separated string (requires `Update`).
    pub args: Option<String>,
    /// Nudge a HostPort's localhost port by this delta (requires `Update`).
    pub port_delta: Option<i32>,
    /// Set a HostPort's localhost port to this value (requires `Update`).
    pub port_set: Option<u16>,
    /// Set a note node's text (requires `Update`).
    pub text: Option<String>,
    /// Point a BindMount node at a host path — a file or a folder (requires
    /// `Update`).
    pub host_path: Option<String>,
    /// Point a MidiIn node at a hardware MIDI input device by name (empty =
    /// the first available) (requires `Update`).
    pub midi_device: Option<String>,
    /// Toggle a Volume node's persistence (bytes saved to a sidecar and
    /// restored on load) (requires `Update`).
    pub persist: Option<bool>,
}

/// A mutation a client asks the server to perform: create/update/delete on a
/// resource, plus the non-CRUD actions (run, duplicate, undo).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Create(Resource),
    Update {
        id: NodeId,
        patch: NodePatch,
    },
    Delete(ResourceRef),
    /// Set where a volume bind (`volume` → `app`) mounts inside the app, e.g.
    /// `/data`. An empty path resets it to the default (the volume's name at the
    /// filesystem root).
    SetMount {
        volume: NodeId,
        app: NodeId,
        path: String,
    },
    /// Set the guest (container) port a serve wire (`served` → `hostport`)
    /// forwards to — the container side of a Docker `host:container` map. `0`
    /// resets it to the HostPort's own port (forward verbatim).
    SetServePort {
        served: NodeId,
        hostport: NodeId,
        container: u16,
    },
    /// Replace an app node's capability token — the Biscuit whose Datalog
    /// decides what the node's wires may grant it (see `wk token`). Empty
    /// bytes reset the node to the workspace's default token.
    SetToken {
        id: NodeId,
        token: Vec<u8>,
    },
    /// (Re)run an idle/exited app node's guest.
    Run(NodeId),
    /// Stop a running app node's guest (it stays placed and can be re-run).
    Stop(NodeId),
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
            Command::Create(Resource::Node { .. } | Resource::HostMount { .. }) => {
                (ResourceKind::Node, Action::Create)
            }
            Command::Create(Resource::Wire { .. }) => (ResourceKind::Wire, Action::Create),
            Command::Create(Resource::Workspace { .. }) => {
                (ResourceKind::Workspace, Action::Create)
            }
            // A patch touching args/port reconfigures the node; pos/size alone
            // is cosmetic layout.
            Command::Update { patch, .. } => {
                if patch.args.is_some()
                    || patch.port_delta.is_some()
                    || patch.port_set.is_some()
                    || patch.host_path.is_some()
                    || patch.persist.is_some()
                {
                    (ResourceKind::Node, Action::Update)
                } else {
                    (ResourceKind::Node, Action::Arrange)
                }
            }
            Command::Delete(ResourceRef::Node(_)) => (ResourceKind::Node, Action::Delete),
            Command::Delete(ResourceRef::Wire(_)) => (ResourceKind::Wire, Action::Delete),
            // Reconfiguring a bind's mount path or a serve's container port
            // modifies that wire.
            Command::SetMount { .. } | Command::SetServePort { .. } => {
                (ResourceKind::Wire, Action::Update)
            }
            Command::Delete(ResourceRef::Workspace(_)) => (ResourceKind::Workspace, Action::Delete),
            // Swapping a node's capability token reconfigures the node.
            Command::SetToken { .. } => (ResourceKind::Node, Action::Update),
            Command::Run(_) | Command::Stop(_) => (ResourceKind::Node, Action::Run),
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
