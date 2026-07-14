//! The wk **server**: the authoritative half of a running workspace. It owns the
//! workspace file, the wasm runtime (`PluginHost` + the fabric + MIDI), and the
//! *document* — every canvas node (app/file/port/network), where each sits, and
//! all the wiring between them. Clients drive it through a `ServerHandle`: they
//! issue mutations and read its state to render.
//!
//! Camera/selection/palette/drag live in the *client*, not here. Node positions
//! and sizes are the server's because they're shared across clients and saved to
//! the workspace file.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::plugin::{NodeRegistry, PluginHost, SharedNode, SharedSurface, SurfaceRegistry};
use crate::wiring::{self, NodeClass};
use crate::workspace::{
    Dependency, Document, IrohState, NetState, NodeState, PortState, Workspace,
};
use wk_protocol::{Command, NodeId, NodeKind, Resource, ResourceRef, Wire};

/// Default canvas size of a file / port / network node, in canvas pixels.
pub const FILE_W: f32 = 130.0;
pub const FILE_H: f32 = 44.0;

/// An in-memory canvas file node: a named shared buffer you wire into app nodes.
pub struct VirtualFile {
    pub name: String,
    pub data: crate::vfs::SharedFile,
}

/// A canvas file node backed by a real file on the host disk.
pub struct HostMappedFile {
    /// In-app mount name (the file's base name).
    pub name: String,
    pub path: PathBuf,
}

/// A canvas file node, wired into app nodes as a shared file `/name`.
pub enum FileNode {
    Virtual(VirtualFile),
    HostMapped(HostMappedFile),
}

impl FileNode {
    /// The in-app file name this node mounts as.
    pub fn name(&self) -> &str {
        match self {
            FileNode::Virtual(f) => &f.name,
            FileNode::HostMapped(f) => &f.name,
        }
    }

    /// Current size in bytes (in-memory length, or the host file's size).
    pub fn size(&self) -> usize {
        match self {
            FileNode::Virtual(f) => f.data.lock().unwrap().len(),
            FileNode::HostMapped(f) => {
                std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0) as usize
            }
        }
    }

    /// Mount this file into app filesystem `fs` (by kind).
    pub fn mount(&self, fs: &crate::vfs::SharedFs) {
        match self {
            FileNode::Virtual(f) => crate::vfs::mount_file(fs, &f.name, f.data.clone()),
            FileNode::HostMapped(f) => crate::vfs::mount_host_file(fs, &f.name, f.path.clone()),
        }
    }
}

/// Render-facing metadata about a file node (the client never touches the live
/// [`FileNode`] behind the server lock).
#[derive(Clone)]
pub struct FileMeta {
    pub name: String,
    pub size: usize,
    pub host_mapped: bool,
}

/// Render-facing metadata about an Iroh uplink node.
#[derive(Clone)]
pub struct IrohMeta {
    /// This uplink's dialable ticket (shown so the user can share it).
    pub ticket: String,
    /// Live tunnel connections.
    pub peers: usize,
}

/// A read-only snapshot of the document a client renders from. Produced by
/// [`Server::view`] under one lock; everything is owned/cloned except the live
/// surface and node handles, which are `Arc`s a client uses to paint pixels and
/// forward input (the in-process fast path; a networked client would receive
/// pixel streams instead).
pub struct View {
    /// Every canvas node id (app/file/port/network), for draw-order reconcile.
    pub node_ids: Vec<NodeId>,
    pub win_pos: HashMap<NodeId, [f32; 2]>,
    pub win_size: HashMap<NodeId, [f32; 2]>,
    pub file_nodes: HashMap<NodeId, FileMeta>,
    pub host_ports: HashMap<NodeId, u16>,
    pub net_nodes: HashSet<NodeId>,
    pub gateways: HashSet<NodeId>,
    pub iroh_nodes: HashMap<NodeId, IrohMeta>,
    pub connections: Vec<(NodeId, NodeId)>,
    pub midi_links: Vec<(NodeId, NodeId)>,
    pub net_links: Vec<(NodeId, NodeId)>,
    /// http node id -> HostPort node id.
    pub serves: HashMap<NodeId, NodeId>,
    /// Per-node launch args (argv after the program name).
    pub node_args: HashMap<NodeId, Vec<String>>,
    /// The launchable dependencies (for the command palette).
    pub available: Vec<Dependency>,
    pub nodes: Vec<SharedNode>,
    pub surfaces: Vec<SharedSurface>,
    /// Which workspace (tab) each node belongs to.
    pub node_ws: HashMap<NodeId, NodeId>,
    /// The workspaces (tabs), in order.
    pub workspaces: Vec<NodeId>,
}

impl View {
    /// The live app node with id `id`, if it is an app (not a file) node.
    pub fn app_node(&self, id: NodeId) -> Option<SharedNode> {
        self.nodes.iter().find(|n| n.id == id).cloned()
    }

    /// Narrow this multi-workspace view down to a single tab, keeping only the
    /// nodes (and wiring between them) that belong to workspace `ws`. Every peer
    /// runs all workspaces; a client renders just the one it is looking at.
    pub fn for_workspace(&self, ws: NodeId) -> View {
        let mine = |id: &NodeId| self.node_ws.get(id).copied() == Some(ws);
        let keep = |m: &HashMap<NodeId, [f32; 2]>| -> HashMap<NodeId, [f32; 2]> {
            m.iter()
                .filter(|(id, _)| mine(id))
                .map(|(&k, &v)| (k, v))
                .collect()
        };
        View {
            node_ids: self
                .node_ids
                .iter()
                .copied()
                .filter(|id| mine(id))
                .collect(),
            win_pos: keep(&self.win_pos),
            win_size: keep(&self.win_size),
            file_nodes: self
                .file_nodes
                .iter()
                .filter(|(id, _)| mine(id))
                .map(|(&k, v)| (k, v.clone()))
                .collect(),
            host_ports: self
                .host_ports
                .iter()
                .filter(|(id, _)| mine(id))
                .map(|(&k, &v)| (k, v))
                .collect(),
            net_nodes: self
                .net_nodes
                .iter()
                .copied()
                .filter(|id| mine(id))
                .collect(),
            gateways: self
                .gateways
                .iter()
                .copied()
                .filter(|id| mine(id))
                .collect(),
            iroh_nodes: self
                .iroh_nodes
                .iter()
                .filter(|(id, _)| mine(id))
                .map(|(&k, v)| (k, v.clone()))
                .collect(),
            connections: self
                .connections
                .iter()
                .copied()
                .filter(|(f, _)| mine(f))
                .collect(),
            midi_links: self
                .midi_links
                .iter()
                .copied()
                .filter(|(s, _)| mine(s))
                .collect(),
            net_links: self
                .net_links
                .iter()
                .copied()
                .filter(|(a, _)| mine(a))
                .collect(),
            serves: self
                .serves
                .iter()
                .filter(|(http, _)| mine(http))
                .map(|(&k, &v)| (k, v))
                .collect(),
            node_args: self
                .node_args
                .iter()
                .filter(|(id, _)| mine(id))
                .map(|(&k, v)| (k, v.clone()))
                .collect(),
            available: self.available.clone(),
            nodes: self.nodes.iter().filter(|n| mine(&n.id)).cloned().collect(),
            surfaces: self.surfaces.clone(),
            node_ws: self.node_ws.clone(),
            workspaces: self.workspaces.clone(),
        }
    }

    /// Whether a given connection currently exists.
    pub fn wire_exists(&self, w: Wire) -> bool {
        match w {
            Wire::File(f, a) => self.connections.contains(&(f, a)),
            Wire::Midi(s, d) => self.midi_links.contains(&(s, d)),
            Wire::Serve(h, hp) => self.serves.get(&h) == Some(&hp),
            Wire::Net(app, net) => self.net_links.contains(&(app, net)),
        }
    }
}

/// The two node ids a [`Wire`] joins.
fn wire_ends(w: Wire) -> (NodeId, NodeId) {
    match w {
        Wire::File(a, b) | Wire::Midi(a, b) | Wire::Serve(a, b) | Wire::Net(a, b) => (a, b),
    }
}

/// The in-app mount name for a host-mapped file: the path's base name.
pub fn host_file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hostfile".to_string())
}

/// Longest undo history kept.
const UNDO_CAP: usize = 200;

/// A recorded inverse of one mutation, applied by [`Command::Undo`].
enum Undo {
    Pos(NodeId, [f32; 2]),
    Size(NodeId, [f32; 2]),
    Args(NodeId, Vec<String>),
    Port(NodeId, u16),
    /// Re-toggle a connection between two nodes (connect is its own inverse).
    Wire(NodeId, NodeId),
    /// Remove a node that a create added.
    Uncreate(NodeId),
    /// Recreate a node that was removed, with its wiring.
    Recreate(Box<Snapshot>),
    /// Remove a workspace tab that an add created.
    DropWorkspace(NodeId),
    /// Recreate a workspace that was removed, with all its nodes and wiring.
    RecreateWorkspace(Box<WsSnapshot>),
}

/// Everything needed to bring a removed workspace tab back exactly as it was.
struct WsSnapshot {
    id: NodeId,
    /// Position in the tab order to restore it at.
    index: usize,
    nodes: Vec<Snapshot>,
}

/// Everything needed to bring a removed node back exactly as it was.
struct Snapshot {
    id: NodeId,
    ws: NodeId,
    pos: [f32; 2],
    size: [f32; 2],
    kind: SnapKind,
    /// Every connection the node was part of, as raw node pairs.
    wires: Vec<(NodeId, NodeId)>,
}

enum SnapKind {
    App {
        dep: String,
        args: Vec<String>,
        options: Vec<f32>,
    },
    Virtual {
        name: String,
        data: Vec<u8>,
    },
    HostFile {
        name: String,
        path: PathBuf,
    },
    Port {
        port: u16,
    },
    Net {
        gateway: bool,
    },
    Iroh {
        secret: [u8; 32],
        /// The peer ticket it was dialing (stored as its args).
        peer: Vec<String>,
    },
}

/// What kind of node this is. The base fact that used to be inferred by probing
/// which parallel map an id lived in (the old `class_of`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    App,
    File,
    Port,
    Network,
    Gateway,
    /// An iroh uplink: extends the Network it's wired to onto a remote fabric.
    Iroh,
}

impl Kind {
    /// A network node is either a plain Network or a Gateway.
    fn is_net(self) -> bool {
        matches!(self, Kind::Network | Kind::Gateway)
    }
}

/// A placed node's base record: its kind, the workspace (tab) it belongs to, and
/// its shared canvas geometry. Kind-specific payload (launch args, file bytes,
/// port number) lives in side tables keyed by the same id.
#[derive(Clone, Copy)]
pub struct NodeRec {
    pub kind: Kind,
    pub ws: NodeId,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

/// The workspace **graph**: the base facts that make up the document — every
/// node, its per-node data, the wiring between nodes, the workspace tabs, and the
/// launchable dependencies. This is the persisted, syncable source of truth; the
/// runtime it drives (live wasm nodes, active effects, undo) lives on [`Server`].
///
/// Both [`Server::view`] (client snapshot) and [`Server::save`] (`.wk` file)
/// project from *this* — there is one representation of the facts.
#[derive(Default)]
pub struct Graph {
    /// Every placed node's base record (kind + workspace + canvas geometry),
    /// keyed by node id. One row per node, kind explicit.
    pub nodes: HashMap<NodeId, NodeRec>,
    /// Per-node launch args (argv after the program name). Side table keyed by id.
    pub node_args: HashMap<NodeId, Vec<String>>,
    /// Canvas file nodes (in-memory or disk-backed) wired into apps.
    pub file_nodes: HashMap<NodeId, FileNode>,
    /// HostPort nodes (canvas id -> localhost port).
    pub host_ports: HashMap<NodeId, u16>,

    /// File connections as (file id, app node id).
    pub connections: Vec<(NodeId, NodeId)>,
    /// MIDI connections as (source node id, destination node id).
    pub midi_links: Vec<(NodeId, NodeId)>,
    /// Serve wiring: (http node id, HostPort id).
    pub serve_links: Vec<(NodeId, NodeId)>,
    /// Network membership wires, as (app node id, Network node id).
    pub net_links: Vec<(NodeId, NodeId)>,

    /// Iroh uplink nodes' ed25519 secrets, so a node's ticket (its dialable
    /// identity) survives restarts. The peer ticket it dials lives in
    /// `node_args`. Side table keyed by node id.
    pub iroh_secrets: HashMap<NodeId, [u8; 32]>,

    /// The workspaces (tabs) in this document, in order — including empty ones.
    pub workspaces: Vec<NodeId>,
    /// The workspace's launchable dependencies.
    pub available: Vec<Dependency>,
}

/// The authoritative running workspace. See the module docs.
pub struct Server {
    pub host: PluginHost,
    /// Surfaces created by wasm nodes (their painted pixels), read by clients.
    pub registry: SurfaceRegistry,
    /// Live wasm nodes.
    pub node_reg: NodeRegistry,
    /// The `.wk` file this workspace loads from and saves back to.
    workspace_path: PathBuf,

    /// The base facts (see [`Graph`]).
    pub graph: Graph,

    // ---- runtime state derived from `graph` (not persisted, not synced) ----
    /// Active file mounts: (file, app) -> (mount name, the app's fs). Stores the
    /// name+fs so a mount can be torn down even after either node is gone.
    /// Mirrors `graph.connections`; reconciled by `sync_mounts`.
    mounted: HashMap<(NodeId, NodeId), (String, crate::vfs::SharedFs)>,
    /// Active MIDI routes: (src, dst) currently in the router. Mirrors
    /// `graph.midi_links`; reconciled by `sync_midi`.
    routed: HashSet<(NodeId, NodeId)>,
    /// Currently *running* servers: served node id -> (HostPort id, kill switch).
    /// A subset of `graph.serve_links` — an entry appears only once the node is
    /// ready (a wasi:http node dispatched per request, or a fabric node with a
    /// TCP forward into its network) and the port bound. Reconciled by
    /// `sync_serves`.
    pub serves: HashMap<NodeId, (NodeId, Arc<AtomicBool>)>,
    /// Running iroh uplinks, one per Iroh node. Dropping one closes its
    /// endpoint and detaches its trunk.
    uplinks: HashMap<NodeId, crate::uplink::Uplink>,

    /// Inverse-command history for [`Command::Undo`].
    undo: Vec<Undo>,

    next_port: u16,
    file_seq: u32,
    host_seq: u32,
}

impl Server {
    /// Create a server and instantiate every workspace in the document (all tabs
    /// run at once). `path` is the `.wk` file to save back to.
    pub fn new(doc: &Document, path: PathBuf) -> Result<Self, String> {
        let host = PluginHost::new().map_err(|e| format!("{e:#}"))?;
        let mut server = Server {
            host,
            registry: Arc::new(Mutex::new(Vec::new())),
            node_reg: Arc::new(Mutex::new(Vec::new())),
            workspace_path: path,
            graph: Graph {
                available: doc.dependencies.clone(),
                workspaces: doc.workspaces.iter().map(|w| w.id).collect(),
                ..Graph::default()
            },
            mounted: HashMap::new(),
            routed: HashSet::new(),
            serves: HashMap::new(),
            uplinks: HashMap::new(),
            undo: Vec::new(),
            next_port: 8080,
            file_seq: 0,
            host_seq: 0,
        };
        for ws in &doc.workspaces {
            server.instantiate(ws);
        }
        Ok(server)
    }

    /// Spawn one workspace's nodes and re-apply its wiring (used at load). Node
    /// positions are set here so every node has a place the moment it exists.
    fn instantiate(&mut self, saved: &Workspace) {
        // App nodes: resolve the dependency by name, spawn with the saved id.
        for n in &saved.nodes {
            let Some(dep) = self
                .graph
                .available
                .iter()
                .find(|d| d.name == n.name)
                .cloned()
            else {
                eprintln!(
                    "workspace references unknown dependency {:?}; skipping",
                    n.name
                );
                continue;
            };
            // The node's saved (possibly-edited) args, else the dependency default.
            let args = if n.args.is_empty() {
                dep.args.clone()
            } else {
                n.args.clone()
            };
            match self.host.spawn(
                &dep.local_path(),
                &dep.name,
                n.id,
                &args,
                self.registry.clone(),
                self.node_reg.clone(),
                n.options.clone(),
            ) {
                Ok(()) => {
                    self.place(n.id, Kind::App, saved.id, n.pos, n.size);
                    self.graph.node_args.insert(n.id, args);
                }
                Err(e) => eprintln!("failed to restore {}: {e:#}", dep.name),
            }
        }

        // VirtualFile nodes: recreate empty shared buffers at their saved spots.
        for f in &saved.virtual_files {
            self.place(f.id, Kind::File, saved.id, f.pos, f.size);
            if let Some(num) = f
                .name
                .strip_prefix("file")
                .and_then(|s| s.parse::<u32>().ok())
            {
                self.file_seq = self.file_seq.max(num);
            }
            self.graph.file_nodes.insert(
                f.id,
                FileNode::Virtual(VirtualFile {
                    name: f.name.clone(),
                    data: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        }

        // HostMappedFile nodes: re-map their saved host paths (name = path).
        for f in &saved.host_files {
            self.place(f.id, Kind::File, saved.id, f.pos, f.size);
            let path = PathBuf::from(&f.name);
            let name = host_file_name(&path);
            if let Some(num) = name
                .strip_prefix("host")
                .and_then(|s| s.parse::<u32>().ok())
            {
                self.host_seq = self.host_seq.max(num);
            }
            self.graph
                .file_nodes
                .insert(f.id, FileNode::HostMapped(HostMappedFile { name, path }));
        }

        // Record desired file connections; `sync_mounts` (below) applies them.
        for &(file_id, app_id) in &saved.connections {
            if self.graph.file_nodes.contains_key(&file_id) && self.app_node(app_id).is_some() {
                self.graph.connections.push((file_id, app_id));
            }
        }

        // Record desired MIDI connections; `sync_midi` (below) routes them.
        for &(src, dst) in &saved.midi {
            if self.app_node(src).is_some() && self.app_node(dst).is_some() {
                self.graph.midi_links.push((src, dst));
            }
        }

        // HostPort nodes: recreate at their saved positions and ports.
        for hp in &saved.host_ports {
            self.next_port = self.next_port.max(hp.port.saturating_add(1));
            self.place(hp.id, Kind::Port, saved.id, hp.pos, hp.size);
            self.graph.host_ports.insert(hp.id, hp.port);
        }

        // Record serve wiring as desired state. The http nodes are still
        // compiling on background threads, so they can't be bound yet; the tick
        // loop's `sync_serves` starts each server once its node is ready. (Binding
        // eagerly here would silently drop the wire, since `http_path()` is not
        // published until compilation finishes — issue that lost serve wires on
        // every load.)
        for &(http_id, hostport_id) in &saved.serves {
            if self.app_node(http_id).is_some() && self.graph.host_ports.contains_key(&hostport_id)
            {
                self.graph.serve_links.push((http_id, hostport_id));
            }
        }

        // Network/Gateway nodes: recreate at their saved spots.
        for net in &saved.nets {
            let kind = if net.gateway {
                Kind::Gateway
            } else {
                Kind::Network
            };
            self.place(net.id, kind, saved.id, net.pos, net.size);
        }

        // Iroh uplink nodes: restart each endpoint with its saved identity (so
        // its ticket is unchanged) and re-dial its saved peer.
        for ir in &saved.irohs {
            let secret = ir
                .secret_bytes()
                .unwrap_or_else(|| iroh::SecretKey::generate().to_bytes());
            self.create_uplink(ir.id, secret, ir.pos, ir.size, saved.id);
            if let Some(peer) = &ir.peer {
                self.set_node_args(ir.id, peer);
            }
        }

        // Re-wire network membership (rejoins the network + grants host access;
        // an uplink member re-trunks its network).
        for &(app_id, net_id) in &saved.net_links {
            if (self.app_node(app_id).is_some() || self.is_iroh(app_id)) && self.is_net(net_id) {
                self.toggle_net(app_id, net_id);
            }
        }

        // Apply the recorded file/MIDI wiring now that every node is placed.
        self.sync_mounts();
        self.sync_midi();
    }

    /// Record a node's base fact: kind, workspace, and canvas geometry.
    fn place(&mut self, id: NodeId, kind: Kind, ws: NodeId, pos: [f32; 2], size: [f32; 2]) {
        self.graph.nodes.insert(
            id,
            NodeRec {
                kind,
                ws,
                pos,
                size,
            },
        );
    }

    /// This node's kind, if it exists.
    fn kind_of(&self, id: NodeId) -> Option<Kind> {
        self.graph.nodes.get(&id).map(|n| n.kind)
    }

    /// Whether `id` is a Network or Gateway node.
    fn is_net(&self, id: NodeId) -> bool {
        self.kind_of(id).is_some_and(Kind::is_net)
    }

    /// Whether `id` is a Gateway node (a Network that grants host access).
    fn is_gateway(&self, id: NodeId) -> bool {
        self.kind_of(id) == Some(Kind::Gateway)
    }

    /// Whether `id` is an Iroh uplink node.
    fn is_iroh(&self, id: NodeId) -> bool {
        self.kind_of(id) == Some(Kind::Iroh)
    }

    fn alloc_id(&mut self) -> NodeId {
        NodeId::new()
    }

    /// Every live canvas node id (app, file, port, network), for a client to
    /// reconcile its stacking order against.
    pub fn node_ids(&self) -> Vec<NodeId> {
        self.graph.nodes.keys().copied().collect()
    }

    /// The live app node with id `id`, if it is an app (not a file) node.
    pub fn app_node(&self, id: NodeId) -> Option<SharedNode> {
        self.node_reg
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
    }

    /// Launch a dependency as a new app node at `pos` in workspace `ws`.
    fn launch(&mut self, dep: &Dependency, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        if let Err(e) = self.host.spawn(
            &dep.local_path(),
            &dep.name,
            id,
            &dep.args,
            self.registry.clone(),
            self.node_reg.clone(),
            Vec::new(),
        ) {
            eprintln!("failed to launch {}: {e:#}", dep.name);
            return;
        }
        self.place(id, Kind::App, ws, pos, [360.0, 260.0]);
        self.graph.node_args.insert(id, dep.args.clone());
    }

    /// Create a new, empty in-memory VirtualFile node at `pos` in workspace `ws`.
    fn add_virtual_file(&mut self, pos: [f32; 2], ws: NodeId) {
        self.file_seq += 1;
        let id = self.alloc_id();
        self.place(id, Kind::File, ws, pos, [FILE_W, FILE_H]);
        self.graph.file_nodes.insert(
            id,
            FileNode::Virtual(VirtualFile {
                name: format!("file{}", self.file_seq),
                data: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    }

    /// Create a HostMappedFile node backed by a fresh host file (`host<n>`).
    fn add_host_mapped_file(&mut self, pos: [f32; 2], ws: NodeId) {
        self.host_seq += 1;
        let id = self.alloc_id();
        let name = format!("host{}", self.host_seq);
        let path = PathBuf::from(&name);
        if let Err(e) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            eprintln!("failed to create host file {}: {e}", path.display());
        }
        self.place(id, Kind::File, ws, pos, [FILE_W, FILE_H]);
        self.graph
            .file_nodes
            .insert(id, FileNode::HostMapped(HostMappedFile { name, path }));
    }

    /// Create a HostPort node at `pos` (auto-assigned localhost port).
    fn add_host_port(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        let port = self.next_port;
        self.next_port = self.next_port.wrapping_add(1).max(8080);
        self.place(id, Kind::Port, ws, pos, [FILE_W, FILE_H]);
        self.graph.host_ports.insert(id, port);
    }

    /// Create a Network node at `pos`; returns its id.
    fn add_net_node(&mut self, pos: [f32; 2], ws: NodeId) -> NodeId {
        let id = self.alloc_id();
        self.place(id, Kind::Network, ws, pos, [FILE_W, FILE_H]);
        id
    }

    /// Create a Gateway node at `pos` (a Network whose members get host access).
    fn add_gateway_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.place(id, Kind::Gateway, ws, pos, [FILE_W, FILE_H]);
    }

    /// Create an Iroh uplink node at `pos` with a fresh identity.
    fn add_iroh_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        let secret = iroh::SecretKey::generate().to_bytes();
        self.create_uplink(id, secret, pos, [FILE_W, FILE_H], ws);
    }

    /// Create (or restore) an Iroh uplink node with a known id and secret. Until
    /// wired to a Network the uplink trunks the node's own (empty) net, so a
    /// connected peer sees nothing.
    fn create_uplink(
        &mut self,
        id: NodeId,
        secret: [u8; 32],
        pos: [f32; 2],
        size: [f32; 2],
        ws: NodeId,
    ) {
        match self.host.uplink(id, Some(secret)) {
            Ok(up) => {
                eprintln!("[iroh] uplink {id} ticket: {}", up.ticket());
                self.uplinks.insert(id, up);
                self.graph.iroh_secrets.insert(id, secret);
                self.place(id, Kind::Iroh, ws, pos, size);
            }
            Err(e) => eprintln!("failed to start iroh uplink: {e:#}"),
        }
    }

    /// Register a new (empty) workspace tab with a client-minted id.
    fn add_workspace(&mut self, id: NodeId) {
        if !self.graph.workspaces.contains(&id) {
            self.graph.workspaces.push(id);
        }
    }

    /// Duplicate a node into the same workspace at an offset. App nodes are
    /// relaunched with their current args + knob settings; wiring isn't copied.
    fn duplicate(&mut self, id: NodeId) {
        let Some(&NodeRec { ws, pos, size, .. }) = self.graph.nodes.get(&id) else {
            return;
        };
        let off = [pos[0] + 40.0, pos[1] + 40.0];

        if let Some(node) = self.app_node(id) {
            let Some(dep) = self
                .graph
                .available
                .iter()
                .find(|d| d.name == node.name)
                .cloned()
            else {
                return;
            };
            let args = self
                .graph
                .node_args
                .get(&id)
                .cloned()
                .unwrap_or_else(|| dep.args.clone());
            let options = node.options.lock().unwrap().clone();
            let new_id = self.alloc_id();
            if let Err(e) = self.host.spawn(
                &dep.local_path(),
                &dep.name,
                new_id,
                &args,
                self.registry.clone(),
                self.node_reg.clone(),
                options,
            ) {
                eprintln!("failed to duplicate {}: {e:#}", dep.name);
                return;
            }
            self.place(new_id, Kind::App, ws, off, size);
            self.graph.node_args.insert(new_id, args);
            return;
        }

        match self
            .graph
            .file_nodes
            .get(&id)
            .map(|f| matches!(f, FileNode::Virtual(_)))
        {
            Some(true) => return self.add_virtual_file(off, ws),
            Some(false) => return self.add_host_mapped_file(off, ws),
            None => {}
        }
        match self.kind_of(id) {
            Some(Kind::Port) => self.add_host_port(off, ws),
            Some(Kind::Gateway) => self.add_gateway_node(off, ws),
            Some(Kind::Network) => {
                self.add_net_node(off, ws);
            }
            // A duplicate uplink is a fresh identity with no peer — tickets
            // are per-endpoint, so there is nothing meaningful to copy.
            Some(Kind::Iroh) => self.add_iroh_node(off, ws),
            _ => {}
        }
    }

    /// Remove a node by kind (app/file/port/network/uplink).
    fn remove_any(&mut self, id: NodeId) {
        match self.kind_of(id) {
            Some(Kind::File) => self.remove_file_node(id),
            Some(Kind::Port) => self.remove_host_port(id),
            Some(Kind::Network | Kind::Gateway) => self.remove_net_node(id),
            Some(Kind::Iroh) => self.remove_iroh_node(id),
            Some(Kind::App) => self.close_node(id),
            None => {}
        }
    }

    /// Delete a workspace and every node in it. A no-op for the last workspace —
    /// a document always keeps at least one.
    fn remove_workspace(&mut self, id: NodeId) {
        if self.graph.workspaces.len() <= 1 || !self.graph.workspaces.contains(&id) {
            return;
        }
        let victims: Vec<NodeId> = self
            .graph
            .nodes
            .iter()
            .filter(|(_, rec)| rec.ws == id)
            .map(|(&n, _)| n)
            .collect();
        for n in victims {
            self.remove_any(n);
        }
        self.graph.workspaces.retain(|&w| w != id);
    }

    /// (Re)run an idle or exited node's guest with its current args.
    fn run_node(&mut self, id: NodeId) {
        if let Some(node) = self.app_node(id) {
            let args = self.graph.node_args.get(&id).cloned().unwrap_or_default();
            if let Err(e) = self.host.run_node(&node, &args) {
                eprintln!("failed to run {}: {e:#}", node.name);
            }
        }
    }

    /// Set a node's launch args from a whitespace-separated string. Guarded to
    /// existing nodes so an `Update` on an unknown id can't grow `node_args`
    /// without bound. For an Iroh uplink the args are its peer ticket — setting
    /// them dials the peer.
    fn set_node_args(&mut self, id: NodeId, text: &str) {
        if !self.graph.nodes.contains_key(&id) {
            return;
        }
        let args = text.split_whitespace().map(str::to_string).collect();
        self.graph.node_args.insert(id, args);
        if let Some(up) = self.uplinks.get(&id) {
            let ticket = text.trim();
            if !ticket.is_empty() {
                if let Err(e) = up.dial(ticket) {
                    eprintln!("[iroh] {e:#}");
                }
            }
        }
    }

    /// Grant/revoke a node's host-network access (on its fabric stack).
    fn set_host_access(&self, app_id: NodeId, allow: bool) {
        if let Some(node) = self.app_node(app_id) {
            if let Some(stack) = node.net_stack() {
                stack.lock().unwrap().host_access = allow;
            }
        }
    }

    /// What kind of node `id` is, for classifying a wire (see [`wiring`]).
    fn class_of(&self, id: NodeId) -> NodeClass {
        match self.kind_of(id) {
            Some(Kind::File) => NodeClass::File,
            Some(Kind::Port) => NodeClass::Port,
            Some(Kind::Network | Kind::Gateway) => NodeClass::Net,
            Some(Kind::Iroh) => NodeClass::Iroh,
            Some(Kind::App) | None => NodeClass::Other,
        }
    }

    /// Toggle a connection between two nodes by their kinds: file⇄app mounts the
    /// file; http-app⇄HostPort serves on localhost; app⇄Network joins the network;
    /// app⇄app wires MIDI. The *decision* (which wire, which orientation) is
    /// [`wiring::classify`]; this only runs the effect for whichever it returns.
    fn connect_toggle(&mut self, a: NodeId, b: NodeId) {
        match wiring::classify(a, b, self.class_of(a), self.class_of(b)) {
            Some(Wire::File(file, app)) => self.toggle_file(file, app),
            Some(Wire::Serve(http, hostport)) => self.toggle_serve(http, hostport),
            Some(Wire::Net(app, net)) => self.toggle_net(app, net),
            Some(Wire::Midi(src, dst)) => self.toggle_midi(src, dst),
            None => {}
        }
    }

    fn set_node_net(&self, app_id: NodeId, net: NodeId) {
        if let Some(node) = self.app_node(app_id) {
            if let Some(stack) = node.net_stack() {
                stack.lock().unwrap().net = net;
            }
        }
    }

    /// Wire (or unwire) app node (or Iroh uplink) `app_id` onto Network node
    /// `net_id`.
    fn toggle_net(&mut self, app_id: NodeId, net_id: NodeId) {
        let joined = wiring::toggle_unique(&mut self.graph.net_links, app_id, net_id);
        // An uplink member: its trunk follows the wire (own empty net = idle).
        if let Some(up) = self.uplinks.get(&app_id) {
            up.set_net(if joined { net_id } else { app_id });
            return;
        }
        if joined {
            // Joined the network (any prior membership was dropped).
            self.set_node_net(app_id, net_id);
            self.set_host_access(app_id, self.is_gateway(net_id));
        } else {
            // Left; back to isolated.
            self.set_node_net(app_id, app_id);
            self.set_host_access(app_id, false);
        }
    }

    /// Ensure each wired node's fabric stack reflects its network membership.
    /// Nodes compile asynchronously, so one wired before its stack existed gets
    /// its membership applied here once it's ready.
    fn sync_net_membership(&self) {
        let nodes = self.node_reg.lock().unwrap().clone();
        for &(app, net) in &self.graph.net_links {
            if let Some(up) = self.uplinks.get(&app) {
                up.set_net(net);
                continue;
            }
            let Some(stack) = nodes
                .iter()
                .find(|n| n.id == app)
                .and_then(|n| n.net_stack())
            else {
                continue;
            };
            let host = self.is_gateway(net);
            let mut g = stack.lock().unwrap();
            if g.net != net || g.host_access != host {
                g.net = net;
                g.host_access = host;
            }
        }
    }

    /// Remove a Network/Gateway node, returning its members to isolation.
    fn remove_net_node(&mut self, id: NodeId) {
        let members: Vec<NodeId> = self
            .graph
            .net_links
            .iter()
            .filter(|&&(_, n)| n == id)
            .map(|&(a, _)| a)
            .collect();
        for app in members {
            if let Some(up) = self.uplinks.get(&app) {
                up.set_net(app);
                continue;
            }
            self.set_node_net(app, app);
            self.set_host_access(app, false);
        }
        self.graph.net_links.retain(|&(_, n)| n != id);
        self.forget(id);
    }

    /// Remove an Iroh uplink node; dropping the uplink closes its endpoint and
    /// detaches its trunk from the fabric.
    fn remove_iroh_node(&mut self, id: NodeId) {
        self.uplinks.remove(&id);
        self.graph.net_links.retain(|&(a, _)| a != id);
        self.forget(id);
    }

    /// Wire (or unwire) an app node to a HostPort. Toggles the *desired* serve
    /// link; the actual bind is (re)established by [`Self::sync_serves`].
    fn toggle_serve(&mut self, http_id: NodeId, hostport_id: NodeId) {
        // "One server per http node" — a new target replaces any existing one.
        wiring::toggle_unique(&mut self.graph.serve_links, http_id, hostport_id);
        self.sync_serves();
    }

    /// Reconcile the running [`Self::serves`] against the desired
    /// [`Self::serve_links`]: stop servers whose wiring changed or whose node/port
    /// went away, and start desired servers that aren't running yet and are now
    /// ready. Idempotent and cheap when nothing changed; called after any serve
    /// change and once per tick (so a wire made before its node finished
    /// compiling is honored as soon as the node comes up).
    fn sync_serves(&mut self) {
        let active: HashMap<NodeId, NodeId> =
            self.serves.iter().map(|(&h, &(hp, _))| (h, hp)).collect();
        let plan = wiring::reconcile_serves(&self.graph.serve_links, &active);
        // Kill the servers to stop, then (re)bind the ones to start. `start_serve`
        // applies its own readiness/port-conflict guards.
        for http in plan.stop {
            if let Some((_, kill)) = self.serves.remove(&http) {
                kill.store(true, Ordering::Relaxed);
            }
        }
        for (http, hostport) in plan.start {
            self.start_serve(http, hostport);
        }
    }

    /// Try to bind the server for one desired serve link. A wasi:http node gets
    /// an HTTP server dispatching into its handler; a fabric (wasi:sockets) node
    /// gets a TCP forward from the localhost port to its fabric address at the
    /// same port number. Silently does nothing if the node isn't ready yet or
    /// its port is already served (both are transient during async compile /
    /// port conflicts); only a real bind failure is logged.
    fn start_serve(&mut self, http_id: NodeId, hostport_id: NodeId) {
        let Some(node) = self.app_node(http_id) else {
            return;
        };
        let Some(&port) = self.graph.host_ports.get(&hostport_id) else {
            return;
        };
        // All workspaces run at once, so another node may already be serving this
        // localhost port. Skip rather than let the OS bind fail; if the other
        // server later stops, a subsequent tick binds this one.
        if self.port_served_by_other(port, http_id) {
            return;
        }
        let kill = Arc::new(AtomicBool::new(false));
        let bound = if let Some(path) = node.http_path() {
            self.host
                .serve(&path, port, Some(node.term_io.clone()), kill.clone())
        } else if let Some(stack) = node.net_stack() {
            self.host.forward(stack, port, kill.clone())
        } else {
            return; // still compiling, or a node with nothing to serve
        };
        if let Err(e) = bound {
            eprintln!("failed to serve {} on :{port}: {e:#}", node.name);
            return;
        }
        self.serves.insert(http_id, (hostport_id, kill));
    }

    /// Whether some *other* http node is already serving localhost `port`.
    fn port_served_by_other(&self, port: u16, except_http: NodeId) -> bool {
        self.serves.iter().any(|(&http, &(hp, _))| {
            http != except_http && self.graph.host_ports.get(&hp) == Some(&port)
        })
    }

    /// Remove a HostPort node, stopping any server bound through it.
    fn remove_host_port(&mut self, id: NodeId) {
        self.graph.host_ports.remove(&id);
        self.graph.serve_links.retain(|&(_, hp)| hp != id);
        self.sync_serves();
        self.forget(id);
    }

    /// Change a HostPort's localhost port by `delta`, live-rebinding any server.
    fn change_port(&mut self, id: NodeId, delta: i32) {
        let Some(&cur) = self.graph.host_ports.get(&id) else {
            return;
        };
        let new = (cur as i32 + delta).clamp(1, 65535) as u16;
        if new == cur {
            return;
        }
        self.graph.host_ports.insert(id, new);
        self.next_port = self.next_port.max(new.saturating_add(1));
        // Stop any server bound through this port; the desired serve link is
        // unchanged (same HostPort id), so `sync_serves` rebinds it on the new
        // port. If the new port collides with another server the rebind is
        // skipped and retried on a later tick — the wire itself is preserved.
        let bound: Vec<NodeId> = self
            .serves
            .iter()
            .filter(|(_, (hp, _))| *hp == id)
            .map(|(&http, _)| http)
            .collect();
        for http in bound {
            if let Some((_, kill)) = self.serves.remove(&http) {
                kill.store(true, Ordering::Relaxed);
            }
        }
        self.sync_serves();
    }

    /// Wire (or unwire) file node `file_id` into app node `app_id`'s filesystem.
    /// Updates the desired `connections` relation; the mount itself is applied by
    /// [`Self::sync_mounts`].
    fn toggle_file(&mut self, file_id: NodeId, app_id: NodeId) {
        wiring::toggle_pair(&mut self.graph.connections, file_id, app_id);
        self.sync_mounts();
    }

    /// Reconcile the actual file mounts against the desired `connections`: mount
    /// each newly-wired file into its app's fs, unmount ones no longer wired.
    /// Idempotent; runs after any connection change and once per tick.
    fn sync_mounts(&mut self) {
        let active: HashSet<(NodeId, NodeId)> = self.mounted.keys().copied().collect();
        let plan = wiring::reconcile_links(&self.graph.connections, &active);
        for pair in plan.remove {
            if let Some((name, fs)) = self.mounted.remove(&pair) {
                crate::vfs::unmount_file(&fs, &name);
            }
        }
        for (file, app) in plan.add {
            let (Some(f), Some(node)) = (self.graph.file_nodes.get(&file), self.app_node(app))
            else {
                continue; // a node isn't resolvable yet — retried next reconcile
            };
            let name = f.name().to_string();
            f.mount(&node.fs);
            self.mounted.insert((file, app), (name, node.fs.clone()));
        }
    }

    /// Wire (or unwire) app node `src`'s MIDI output into app node `dst`'s input.
    /// Updates the desired `midi_links` relation; routing is applied by
    /// [`Self::sync_midi`].
    fn toggle_midi(&mut self, src: NodeId, dst: NodeId) {
        wiring::toggle_pair(&mut self.graph.midi_links, src, dst);
        self.sync_midi();
    }

    /// Reconcile the MIDI router against the desired `midi_links`: add each new
    /// route (once its destination exists), drop routes no longer wired.
    fn sync_midi(&mut self) {
        let plan = wiring::reconcile_links(&self.graph.midi_links, &self.routed);
        let router = self.host.midi();
        let mut routes = router.lock().unwrap();
        for (src, dst) in plan.remove {
            routes.disconnect(src, dst);
            self.routed.remove(&(src, dst));
        }
        for (src, dst) in plan.add {
            if let Some(dst_node) = self.app_node(dst) {
                routes.connect(src, dst, dst_node.midi_in.clone());
                self.routed.insert((src, dst));
            }
        }
    }

    /// Remove a file node; `sync_mounts` unmounts it from every app it was
    /// connected to (using the stored mount handles, so it works after the node
    /// is gone).
    fn remove_file_node(&mut self, id: NodeId) {
        self.graph.connections.retain(|&(f, _)| f != id);
        self.sync_mounts();
        self.forget(id);
    }

    /// Drop a removed node's canvas geometry.
    /// Drop a node's base record and every side-table entry keyed by it, so no
    /// path can leave an orphan (args/file/port) behind a removed node.
    fn forget(&mut self, id: NodeId) {
        self.graph.nodes.remove(&id);
        self.graph.node_args.remove(&id);
        self.graph.file_nodes.remove(&id);
        self.graph.host_ports.remove(&id);
        self.graph.iroh_secrets.remove(&id);
    }

    /// Whether the given wire still connects two live nodes.
    pub fn wire_exists(&self, w: Wire) -> bool {
        match w {
            Wire::File(f, a) => self.graph.connections.contains(&(f, a)),
            Wire::Midi(s, d) => self.graph.midi_links.contains(&(s, d)),
            Wire::Serve(h, hp) => self.graph.serve_links.contains(&(h, hp)),
            Wire::Net(app, net) => self.graph.net_links.contains(&(app, net)),
        }
    }

    /// Remove the given connection (the same effect as toggling it off).
    fn disconnect_wire(&mut self, w: Wire) {
        match w {
            Wire::File(f, a) => {
                if self.graph.connections.contains(&(f, a)) {
                    self.toggle_file(f, a);
                }
            }
            Wire::Midi(s, d) => {
                if self.graph.midi_links.contains(&(s, d)) {
                    self.toggle_midi(s, d);
                }
            }
            Wire::Serve(h, hp) => {
                if self.graph.serve_links.contains(&(h, hp)) {
                    self.toggle_serve(h, hp);
                }
            }
            Wire::Net(app, net) => {
                if self.graph.net_links.contains(&(app, net)) {
                    self.toggle_net(app, net);
                }
            }
        }
    }

    /// Move / resize a node. Guarded to existing nodes so an `Update` naming an
    /// unknown id can't insert phantom geometry that never gets cleaned up (and
    /// would make `node_exists` report a node that was never created).
    fn set_node_pos(&mut self, id: NodeId, pos: [f32; 2]) {
        if let Some(rec) = self.graph.nodes.get_mut(&id) {
            rec.pos = pos;
        }
    }
    fn set_node_size(&mut self, id: NodeId, size: [f32; 2]) {
        if let Some(rec) = self.graph.nodes.get_mut(&id) {
            rec.size = size;
        }
    }

    /// One server step: reconcile any wiring that was pending on a still-loading
    /// node. Cheap; a client calls it each frame, headless in its tick loop.
    pub fn tick(&mut self) {
        self.sync_mounts();
        self.sync_midi();
        self.sync_net_membership();
        self.sync_serves();
    }

    /// Kill a node and drop everything referencing it (its wiring, geometry, and
    /// the wasm instance). Used when a client closes a node.
    fn close_node(&mut self, id: NodeId) {
        if let Some(node) = self.app_node(id) {
            node.kill.store(true, Ordering::Relaxed);
            node.term_io.close();
            if let Some(stack) = &node.net_stack() {
                self.host.detach_net(stack);
            }
        }
        self.registry.lock().unwrap().retain(|s| {
            let mut g = s.lock().unwrap();
            if g.node_id != id {
                return true;
            }
            g.closed = true;
            g.wake();
            false
        });
        self.node_reg.lock().unwrap().retain(|x| x.id != id);
        // Drop every wire touching this node from the desired relations, then
        // reconcile so the corresponding effects (mounts, routes, servers) are
        // torn down. (Its net stack was already detached above.)
        self.graph.connections.retain(|&(_, app)| app != id);
        self.graph.net_links.retain(|&(app, _)| app != id);
        self.graph.midi_links.retain(|&(s, d)| s != id && d != id);
        self.graph
            .serve_links
            .retain(|&(h, hp)| h != id && hp != id);
        self.sync_mounts();
        self.sync_midi();
        self.sync_serves();
        self.forget(id);
    }

    /// Snapshot every workspace into a [`Document`] and write it back to disk.
    pub fn save(&self) {
        let node_list = self.node_reg.lock().unwrap().clone();
        let workspaces = self
            .graph
            .workspaces
            .iter()
            .map(|&ws_id| {
                let mine = |id: &NodeId| self.graph.nodes.get(id).map(|n| n.ws) == Some(ws_id);
                Workspace {
                    id: ws_id,
                    nodes: node_list
                        .iter()
                        .filter(|n| mine(&n.id))
                        .filter_map(|node| {
                            Some(NodeState {
                                name: node.name.clone(),
                                id: node.id,
                                pos: self.graph.nodes.get(&node.id)?.pos,
                                size: self.graph.nodes.get(&node.id)?.size,
                                options: node.options.lock().unwrap().clone(),
                                args: self
                                    .graph
                                    .node_args
                                    .get(&node.id)
                                    .cloned()
                                    .unwrap_or_default(),
                            })
                        })
                        .collect(),
                    virtual_files: self
                        .graph
                        .file_nodes
                        .iter()
                        .filter(|(id, _)| mine(id))
                        .filter_map(|(&id, f)| match f {
                            FileNode::Virtual(v) => Some(NodeState {
                                name: v.name.clone(),
                                id,
                                pos: self.graph.nodes.get(&id)?.pos,
                                size: self.graph.nodes.get(&id)?.size,
                                options: Vec::new(),
                                args: Vec::new(),
                            }),
                            FileNode::HostMapped(_) => None,
                        })
                        .collect(),
                    host_files: self
                        .graph
                        .file_nodes
                        .iter()
                        .filter(|(id, _)| mine(id))
                        .filter_map(|(&id, f)| match f {
                            FileNode::HostMapped(h) => Some(NodeState {
                                name: h.path.to_string_lossy().into_owned(),
                                id,
                                pos: self.graph.nodes.get(&id)?.pos,
                                size: self.graph.nodes.get(&id)?.size,
                                options: Vec::new(),
                                args: Vec::new(),
                            }),
                            FileNode::Virtual(_) => None,
                        })
                        .collect(),
                    host_ports: self
                        .graph
                        .host_ports
                        .iter()
                        .filter(|(id, _)| mine(id))
                        .filter_map(|(&id, &port)| {
                            Some(PortState {
                                id,
                                port,
                                pos: self.graph.nodes.get(&id)?.pos,
                                size: self.graph.nodes.get(&id)?.size,
                            })
                        })
                        .collect(),
                    connections: self
                        .graph
                        .connections
                        .iter()
                        .filter(|(f, _)| mine(f))
                        .copied()
                        .collect(),
                    midi: self
                        .graph
                        .midi_links
                        .iter()
                        .filter(|(s, _)| mine(s))
                        .copied()
                        .collect(),
                    serves: self
                        .graph
                        .serve_links
                        .iter()
                        .filter(|(http, _)| mine(http))
                        .copied()
                        .collect(),
                    nets: self
                        .graph
                        .nodes
                        .iter()
                        .filter(|(id, rec)| rec.kind.is_net() && mine(id))
                        .map(|(&id, rec)| NetState {
                            id,
                            gateway: rec.kind == Kind::Gateway,
                            pos: rec.pos,
                            size: rec.size,
                        })
                        .collect(),
                    net_links: self
                        .graph
                        .net_links
                        .iter()
                        .filter(|(a, _)| mine(a))
                        .copied()
                        .collect(),
                    irohs: self
                        .graph
                        .iroh_secrets
                        .iter()
                        .filter(|(id, _)| mine(id))
                        .filter_map(|(&id, secret)| {
                            Some(IrohState {
                                id,
                                secret: Some(IrohState::secret_hex(secret)),
                                peer: self
                                    .graph
                                    .node_args
                                    .get(&id)
                                    .filter(|a| !a.is_empty())
                                    .map(|a| a.join(" ")),
                                pos: self.graph.nodes.get(&id)?.pos,
                                size: self.graph.nodes.get(&id)?.size,
                            })
                        })
                        .collect(),
                }
            })
            .collect();
        let doc = Document {
            dependencies: self.graph.available.clone(),
            workspaces,
        };
        if let Err(e) = doc.save(&self.workspace_path) {
            eprintln!("failed to save workspace: {e}");
        }
    }

    /// Apply a client [`Command`], recording an inverse for [`Command::Undo`]
    /// where the mutation is undoable. The single entry point for mutations.
    pub fn apply(&mut self, cmd: Command) {
        match &cmd {
            // Node creates: run, then record removal of whatever node appeared.
            Command::Create(Resource::Node { .. }) | Command::Duplicate(_) => {
                let before: HashSet<NodeId> = self.graph.nodes.keys().copied().collect();
                self.dispatch(cmd);
                let created: Vec<NodeId> = self
                    .graph
                    .nodes
                    .keys()
                    .copied()
                    .filter(|id| !before.contains(id))
                    .collect();
                for id in created {
                    self.record(Undo::Uncreate(id));
                }
                return;
            }
            Command::Create(Resource::Wire { a, b }) => {
                // Only record when the create will actually connect.
                if !self.wired(*a, *b) {
                    self.record(Undo::Wire(*a, *b));
                }
            }
            Command::Create(Resource::Workspace { id }) => {
                if !self.graph.workspaces.contains(id) {
                    self.record(Undo::DropWorkspace(*id));
                }
            }
            Command::Update { id, patch } => {
                if patch.pos.is_some() {
                    if let Some(rec) = self.graph.nodes.get(id) {
                        self.record(Undo::Pos(*id, rec.pos));
                    }
                }
                if patch.size.is_some() {
                    if let Some(rec) = self.graph.nodes.get(id) {
                        self.record(Undo::Size(*id, rec.size));
                    }
                }
                if patch.args.is_some() {
                    let old = self.graph.node_args.get(id).cloned().unwrap_or_default();
                    self.record(Undo::Args(*id, old));
                }
                if patch.port_delta.is_some() {
                    if let Some(&p) = self.graph.host_ports.get(id) {
                        self.record(Undo::Port(*id, p));
                    }
                }
            }
            Command::Delete(ResourceRef::Node(id)) => {
                if let Some(s) = self.snapshot(*id) {
                    self.record(Undo::Recreate(Box::new(s)));
                }
            }
            Command::Delete(ResourceRef::Wire(w)) => {
                if self.wire_exists(*w) {
                    let (a, b) = wire_ends(*w);
                    self.record(Undo::Wire(a, b));
                }
            }
            Command::Delete(ResourceRef::Workspace(id)) => {
                if self.graph.workspaces.len() > 1 && self.graph.workspaces.contains(id) {
                    if let Some(s) = self.snapshot_workspace(*id) {
                        self.record(Undo::RecreateWorkspace(Box::new(s)));
                    }
                }
            }
            // Not undoable: run and undo itself.
            Command::Run(_) | Command::Undo => {}
        }
        self.dispatch(cmd);
    }

    /// Perform a command's mutation (no undo recording).
    fn dispatch(&mut self, cmd: Command) {
        match cmd {
            Command::Create(Resource::Node { kind, pos, ws }) => match kind {
                NodeKind::App { dep } => {
                    if let Some(dep) = self.graph.available.get(dep).cloned() {
                        self.launch(&dep, pos, ws);
                    }
                }
                NodeKind::VirtualFile => self.add_virtual_file(pos, ws),
                NodeKind::HostFile => self.add_host_mapped_file(pos, ws),
                NodeKind::Port => self.add_host_port(pos, ws),
                NodeKind::Network => {
                    self.add_net_node(pos, ws);
                }
                NodeKind::Gateway => self.add_gateway_node(pos, ws),
                NodeKind::Iroh => self.add_iroh_node(pos, ws),
            },
            // Create is create only: a wire that already exists is left alone
            // (removal is Delete, so a create-only token can never disconnect).
            Command::Create(Resource::Wire { a, b }) => {
                if !self.wired(a, b) {
                    self.connect_toggle(a, b);
                }
            }
            Command::Create(Resource::Workspace { id }) => self.add_workspace(id),
            Command::Update { id, patch } => {
                if let Some(pos) = patch.pos {
                    self.set_node_pos(id, pos);
                }
                if let Some(size) = patch.size {
                    self.set_node_size(id, size);
                }
                if let Some(args) = patch.args {
                    self.set_node_args(id, &args);
                }
                if let Some(delta) = patch.port_delta {
                    self.change_port(id, delta);
                }
            }
            Command::Delete(ResourceRef::Node(id)) => self.remove_any(id),
            Command::Delete(ResourceRef::Wire(w)) => self.disconnect_wire(w),
            Command::Delete(ResourceRef::Workspace(id)) => self.remove_workspace(id),
            Command::Run(id) => self.run_node(id),
            Command::Duplicate(id) => self.duplicate(id),
            Command::Undo => {
                if let Some(u) = self.undo.pop() {
                    self.apply_undo(u);
                }
            }
        }
    }

    /// Push an inverse onto the undo stack, coalescing a run of same-node
    /// move/resize/args edits (e.g. a drag) into a single entry.
    fn record(&mut self, u: Undo) {
        let coalesce = match (self.undo.last(), &u) {
            (Some(Undo::Pos(a, _)), Undo::Pos(b, _)) => a == b,
            (Some(Undo::Size(a, _)), Undo::Size(b, _)) => a == b,
            (Some(Undo::Args(a, _)), Undo::Args(b, _)) => a == b,
            _ => false,
        };
        if coalesce {
            return;
        }
        self.undo.push(u);
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
    }

    /// Whether a node with this id currently exists (any kind).
    fn node_exists(&self, id: NodeId) -> bool {
        self.graph.nodes.contains_key(&id)
    }

    /// Apply one recorded inverse. Guards against nodes that have since gone.
    fn apply_undo(&mut self, u: Undo) {
        match u {
            Undo::Pos(id, p) => self.set_node_pos(id, p),
            Undo::Size(id, s) => self.set_node_size(id, s),
            Undo::Args(id, a) => {
                if self.graph.node_args.contains_key(&id) {
                    self.graph.node_args.insert(id, a);
                }
            }
            Undo::Port(id, port) => {
                if let Some(&cur) = self.graph.host_ports.get(&id) {
                    self.change_port(id, port as i32 - cur as i32);
                }
            }
            Undo::Wire(a, b) => {
                if self.node_exists(a) && self.node_exists(b) {
                    self.connect_toggle(a, b);
                }
            }
            Undo::Uncreate(id) => {
                if self.node_exists(id) {
                    self.remove_any(id);
                }
            }
            Undo::Recreate(s) => self.recreate(*s),
            Undo::DropWorkspace(id) => self.remove_workspace(id),
            Undo::RecreateWorkspace(s) => self.recreate_workspace(*s),
        }
    }

    /// Capture everything needed to bring node `id` back after removal.
    fn snapshot(&self, id: NodeId) -> Option<Snapshot> {
        let &NodeRec { ws, pos, size, .. } = self.graph.nodes.get(&id)?;
        let kind = if let Some(node) = self.app_node(id) {
            SnapKind::App {
                dep: node.name.clone(),
                args: self.graph.node_args.get(&id).cloned().unwrap_or_default(),
                options: node.options.lock().unwrap().clone(),
            }
        } else if let Some(f) = self.graph.file_nodes.get(&id) {
            match f {
                FileNode::Virtual(v) => SnapKind::Virtual {
                    name: v.name.clone(),
                    data: v.data.lock().unwrap().clone(),
                },
                FileNode::HostMapped(h) => SnapKind::HostFile {
                    name: h.name.clone(),
                    path: h.path.clone(),
                },
            }
        } else if let Some(&port) = self.graph.host_ports.get(&id) {
            SnapKind::Port { port }
        } else if self.is_net(id) {
            SnapKind::Net {
                gateway: self.is_gateway(id),
            }
        } else if let Some(&secret) = self.graph.iroh_secrets.get(&id) {
            SnapKind::Iroh {
                secret,
                peer: self.graph.node_args.get(&id).cloned().unwrap_or_default(),
            }
        } else {
            return None;
        };
        let mut wires: Vec<(NodeId, NodeId)> = Vec::new();
        wires.extend(
            self.graph
                .connections
                .iter()
                .filter(|&&(f, a)| f == id || a == id),
        );
        wires.extend(
            self.graph
                .midi_links
                .iter()
                .filter(|&&(s, d)| s == id || d == id),
        );
        wires.extend(
            self.graph
                .serve_links
                .iter()
                .filter(|&&(h, hp)| h == id || hp == id)
                .copied(),
        );
        wires.extend(
            self.graph
                .net_links
                .iter()
                .filter(|&&(a, n)| a == id || n == id),
        );
        Some(Snapshot {
            id,
            ws,
            pos,
            size,
            kind,
            wires,
        })
    }

    /// Bring a removed node back with the same id, then re-establish its wiring.
    fn recreate(&mut self, s: Snapshot) {
        self.recreate_node(&s);
        self.rewire(&s.wires);
    }

    /// Bring a removed node back with the same id (no wiring yet).
    fn recreate_node(&mut self, s: &Snapshot) {
        match &s.kind {
            SnapKind::App { dep, args, options } => {
                let Some(d) = self
                    .graph
                    .available
                    .iter()
                    .find(|x| &x.name == dep)
                    .cloned()
                else {
                    return;
                };
                if self
                    .host
                    .spawn(
                        &d.local_path(),
                        &d.name,
                        s.id,
                        args,
                        self.registry.clone(),
                        self.node_reg.clone(),
                        options.clone(),
                    )
                    .is_err()
                {
                    return;
                }
                self.place(s.id, Kind::App, s.ws, s.pos, s.size);
                self.graph.node_args.insert(s.id, args.clone());
            }
            SnapKind::Virtual { name, data } => {
                self.place(s.id, Kind::File, s.ws, s.pos, s.size);
                self.graph.file_nodes.insert(
                    s.id,
                    FileNode::Virtual(VirtualFile {
                        name: name.clone(),
                        data: Arc::new(Mutex::new(data.clone())),
                    }),
                );
            }
            SnapKind::HostFile { name, path } => {
                self.place(s.id, Kind::File, s.ws, s.pos, s.size);
                self.graph.file_nodes.insert(
                    s.id,
                    FileNode::HostMapped(HostMappedFile {
                        name: name.clone(),
                        path: path.clone(),
                    }),
                );
            }
            SnapKind::Port { port } => {
                self.place(s.id, Kind::Port, s.ws, s.pos, s.size);
                self.graph.host_ports.insert(s.id, *port);
            }
            SnapKind::Net { gateway } => {
                let kind = if *gateway {
                    Kind::Gateway
                } else {
                    Kind::Network
                };
                self.place(s.id, kind, s.ws, s.pos, s.size);
            }
            SnapKind::Iroh { secret, peer } => {
                self.create_uplink(s.id, *secret, s.pos, s.size, s.ws);
                if !peer.is_empty() {
                    self.set_node_args(s.id, &peer.join(" "));
                }
            }
        }
    }

    /// Whether two nodes are already joined by any connection.
    fn wired(&self, a: NodeId, b: NodeId) -> bool {
        let pair = |x: NodeId, y: NodeId| (x == a && y == b) || (x == b && y == a);
        self.graph.connections.iter().any(|&(x, y)| pair(x, y))
            || self.graph.midi_links.iter().any(|&(x, y)| pair(x, y))
            || self.graph.net_links.iter().any(|&(x, y)| pair(x, y))
            || self.graph.serve_links.iter().any(|&(h, hp)| pair(h, hp))
    }

    /// Re-establish connections between live nodes (idempotent, so a wire listed
    /// twice isn't toggled back off).
    fn rewire(&mut self, wires: &[(NodeId, NodeId)]) {
        for &(a, b) in wires {
            if self.node_exists(a) && self.node_exists(b) && !self.wired(a, b) {
                self.connect_toggle(a, b);
            }
        }
    }

    /// Capture a whole workspace tab (its position + every node) for undo.
    fn snapshot_workspace(&self, ws: NodeId) -> Option<WsSnapshot> {
        let index = self.graph.workspaces.iter().position(|&w| w == ws)?;
        let nodes = self
            .graph
            .nodes
            .iter()
            .filter(|(_, rec)| rec.ws == ws)
            .filter_map(|(&id, _)| self.snapshot(id))
            .collect();
        Some(WsSnapshot {
            id: ws,
            index,
            nodes,
        })
    }

    /// Bring a removed workspace back: its tab, all its nodes, then their wiring.
    fn recreate_workspace(&mut self, s: WsSnapshot) {
        if !self.graph.workspaces.contains(&s.id) {
            let i = s.index.min(self.graph.workspaces.len());
            self.graph.workspaces.insert(i, s.id);
        }
        for node in &s.nodes {
            self.recreate_node(node);
        }
        for node in &s.nodes {
            self.rewire(&node.wires);
        }
    }

    /// A read-only snapshot of everything a client needs to render this frame.
    /// Taken under a single lock by the runtime and handed to clients so none of
    /// them holds a live lock on the server (and so the shape is exactly what a
    /// networked client would receive over the wire).
    pub fn view(&self) -> View {
        let nodes: Vec<SharedNode> = self.node_reg.lock().unwrap().clone();
        let surfaces: Vec<SharedSurface> = self.registry.lock().unwrap().clone();
        let file_nodes = self
            .graph
            .file_nodes
            .iter()
            .map(|(&id, f)| {
                (
                    id,
                    FileMeta {
                        name: f.name().to_string(),
                        size: f.size(),
                        host_mapped: matches!(f, FileNode::HostMapped(_)),
                    },
                )
            })
            .collect();
        // Show the desired wiring (what the user drew and what we persist), not
        // just servers that have finished binding.
        let serves = self.graph.serve_links.iter().copied().collect();
        // Project the normalized node table back into the per-attribute maps the
        // client View exposes (kept flat so the compositor is unchanged).
        let win_pos = self
            .graph
            .nodes
            .iter()
            .map(|(&id, r)| (id, r.pos))
            .collect();
        let win_size = self
            .graph
            .nodes
            .iter()
            .map(|(&id, r)| (id, r.size))
            .collect();
        let node_ws = self.graph.nodes.iter().map(|(&id, r)| (id, r.ws)).collect();
        let net_nodes = self
            .graph
            .nodes
            .iter()
            .filter(|(_, r)| r.kind.is_net())
            .map(|(&id, _)| id)
            .collect();
        let gateways = self
            .graph
            .nodes
            .iter()
            .filter(|(_, r)| r.kind == Kind::Gateway)
            .map(|(&id, _)| id)
            .collect();
        let iroh_nodes = self
            .uplinks
            .iter()
            .map(|(&id, up)| {
                (
                    id,
                    IrohMeta {
                        ticket: up.ticket().to_string(),
                        peers: up.peers(),
                    },
                )
            })
            .collect();
        View {
            node_ids: self.node_ids(),
            win_pos,
            win_size,
            file_nodes,
            host_ports: self.graph.host_ports.clone(),
            net_nodes,
            gateways,
            iroh_nodes,
            connections: self.graph.connections.clone(),
            midi_links: self.graph.midi_links.clone(),
            net_links: self.graph.net_links.clone(),
            serves,
            node_args: self.graph.node_args.clone(),
            available: self.graph.available.clone(),
            nodes,
            surfaces,
            node_ws,
            workspaces: self.graph.workspaces.clone(),
        }
    }
}

#[cfg(test)]
mod model_tests {
    //! Property-based model test of the command/undo state machine. A `Server` is
    //! expensive to build (engine + gpu global + hub thread), so this uses a
    //! modest case count. It exercises only the wasm-free node kinds (file, port,
    //! network, gateway) so no real plugin has to be compiled; app-node creation
    //! and wiring (which need real wasm) are out of scope here.

    use super::*;
    use proptest::prelude::*;
    use wk_protocol::NodePatch;

    fn fresh_server() -> Server {
        Server::new(&Document::empty(), PathBuf::from("wk-proptest-scratch.wk"))
            .expect("a headless server constructs")
    }

    /// Two servers each grow an Iroh node wired to a Network; pasting one's
    /// ticket into the other (the args patch) establishes a live tunnel — the
    /// whole client path (palette create → wire → paste → dial) minus pixels.
    #[test]
    fn iroh_nodes_wire_and_dial_between_servers() {
        let mut a = fresh_server();
        let mut b = fresh_server();
        let setup = |s: &mut Server| {
            let ws = s.graph.workspaces[0];
            s.apply(Command::Create(Resource::Node {
                kind: NodeKind::Iroh,
                pos: [0.0, 0.0],
                ws,
            }));
            s.apply(Command::Create(Resource::Node {
                kind: NodeKind::Network,
                pos: [100.0, 0.0],
                ws,
            }));
            let iroh = *s.graph.iroh_secrets.keys().next().expect("iroh node");
            let net = s
                .graph
                .nodes
                .iter()
                .find(|(_, r)| r.kind == Kind::Network)
                .map(|(&id, _)| id)
                .expect("network node");
            s.apply(Command::Create(Resource::Wire { a: iroh, b: net }));
            assert!(s.graph.net_links.contains(&(iroh, net)));
            iroh
        };
        let ia = setup(&mut a);
        let ib = setup(&mut b);
        let ticket = a.view().iroh_nodes[&ia].ticket.clone();

        b.apply(Command::Update {
            id: ib,
            patch: NodePatch {
                args: Some(ticket),
                ..Default::default()
            },
        });

        // The dialer retries on a 2s cadence; allow a few rounds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (pa, pb) = (
                a.view().iroh_nodes[&ia].peers,
                b.view().iroh_nodes[&ib].peers,
            );
            if pa == 1 && pb == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "uplinks never connected (peers: a={pa} b={pb})"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// The `i`-th live node id (order-stabilized), or `None` when empty. Lets an
    /// op reference "some existing node" without knowing the server-minted ids.
    fn nth_live(s: &Server, i: usize) -> Option<NodeId> {
        let mut ids = s.node_ids();
        if ids.is_empty() {
            return None;
        }
        ids.sort();
        Some(ids[i % ids.len()])
    }

    #[derive(Clone, Debug)]
    enum Op {
        CreateFile,
        CreatePort,
        CreateNet,
        CreateGateway,
        Move(usize, f32, f32),
        Resize(usize, f32, f32),
        SetArgs(usize, String),
        Delete(usize),
        Duplicate(usize),
        /// `Update` a (near-certainly) non-existent id — must not create phantom
        /// geometry.
        UpdateGhost(u128),
        Undo,
    }

    fn op_strat() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::CreateFile),
            Just(Op::CreatePort),
            Just(Op::CreateNet),
            Just(Op::CreateGateway),
            (any::<usize>(), -1.0e5f32..1.0e5, -1.0e5f32..1.0e5)
                .prop_map(|(i, x, y)| Op::Move(i, x, y)),
            (any::<usize>(), 1.0e2f32..1.0e4, 1.0e2f32..1.0e4)
                .prop_map(|(i, w, h)| Op::Resize(i, w, h)),
            (any::<usize>(), "[a-z ]{0,8}").prop_map(|(i, a)| Op::SetArgs(i, a)),
            any::<usize>().prop_map(Op::Delete),
            any::<usize>().prop_map(Op::Duplicate),
            any::<u128>().prop_map(Op::UpdateGhost),
            Just(Op::Undo),
        ]
    }

    fn apply_op(s: &mut Server, op: &Op) {
        let ws = s.graph.workspaces[0];
        let create = |kind| {
            Command::Create(Resource::Node {
                kind,
                pos: [10.0, 20.0],
                ws,
            })
        };
        match op {
            Op::CreateFile => s.apply(create(NodeKind::VirtualFile)),
            Op::CreatePort => s.apply(create(NodeKind::Port)),
            Op::CreateNet => s.apply(create(NodeKind::Network)),
            Op::CreateGateway => s.apply(create(NodeKind::Gateway)),
            Op::Move(i, x, y) => {
                if let Some(id) = nth_live(s, *i) {
                    s.apply(Command::Update {
                        id,
                        patch: NodePatch {
                            pos: Some([*x, *y]),
                            ..Default::default()
                        },
                    });
                }
            }
            Op::Resize(i, w, h) => {
                if let Some(id) = nth_live(s, *i) {
                    s.apply(Command::Update {
                        id,
                        patch: NodePatch {
                            size: Some([*w, *h]),
                            ..Default::default()
                        },
                    });
                }
            }
            Op::SetArgs(i, a) => {
                if let Some(id) = nth_live(s, *i) {
                    s.apply(Command::Update {
                        id,
                        patch: NodePatch {
                            args: Some(a.clone()),
                            ..Default::default()
                        },
                    });
                }
            }
            Op::Delete(i) => {
                if let Some(id) = nth_live(s, *i) {
                    s.apply(Command::Delete(ResourceRef::Node(id)));
                }
            }
            Op::Duplicate(i) => {
                if let Some(id) = nth_live(s, *i) {
                    s.apply(Command::Duplicate(id));
                }
            }
            Op::UpdateGhost(n) => s.apply(Command::Update {
                id: NodeId::from_u128(*n),
                patch: NodePatch {
                    pos: Some([1.0, 2.0]),
                    size: Some([3.0, 4.0]),
                    args: Some("ghost".into()),
                    port_delta: None,
                },
            }),
            Op::Undo => s.apply(Command::Undo),
        }
    }

    /// Core state invariant after normalization: the node table is exactly the
    /// set of live nodes, no side table (args/files/ports) holds an entry for a
    /// node not in the table, and the document keeps at least one workspace.
    fn assert_consistent(s: &Server) -> Result<(), TestCaseError> {
        let base: HashSet<NodeId> = s.graph.nodes.keys().copied().collect();
        let live: HashSet<NodeId> = s.node_ids().into_iter().collect();
        prop_assert_eq!(
            &base,
            &live,
            "node table and live-node enumeration diverged"
        );
        for id in s.graph.node_args.keys() {
            prop_assert!(base.contains(id), "orphan node_args entry");
        }
        for id in s.graph.file_nodes.keys() {
            prop_assert!(base.contains(id), "orphan file_nodes entry");
        }
        for id in s.graph.host_ports.keys() {
            prop_assert!(base.contains(id), "orphan host_ports entry");
        }
        prop_assert!(
            !s.graph.workspaces.is_empty(),
            "document lost its last workspace"
        );
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        /// Any sequence of node create/move/resize/delete/duplicate/undo commands
        /// (including updates to unknown ids) leaves the server's per-node maps
        /// mutually consistent after every step.
        #[test]
        fn node_lifecycle_keeps_state_consistent(
            ops in prop::collection::vec(op_strat(), 0..40),
        ) {
            let mut s = fresh_server();
            assert_consistent(&s)?;
            for op in &ops {
                apply_op(&mut s, op);
                s.tick();
                assert_consistent(&s)?;
            }
        }
    }
}
