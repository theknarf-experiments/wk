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
use crate::workspace::{Dependency, Document, NetState, NodeState, PortState, Workspace};
use wk_protocol::{Command, NodeId, Wire};

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

/// The in-app mount name for a host-mapped file: the path's base name.
pub fn host_file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hostfile".to_string())
}

/// The authoritative running workspace. See the module docs.
pub struct Server {
    pub host: PluginHost,
    /// Surfaces created by wasm nodes (their painted pixels), read by clients.
    pub registry: SurfaceRegistry,
    /// Live wasm nodes.
    pub node_reg: NodeRegistry,
    /// The workspace's launchable dependencies.
    pub available: Vec<Dependency>,
    /// The `.wk` file this workspace loads from and saves back to.
    workspace_path: PathBuf,

    /// Node positions/sizes, keyed by node id (shared canvas geometry).
    pub win_pos: HashMap<NodeId, [f32; 2]>,
    pub win_size: HashMap<NodeId, [f32; 2]>,
    /// Per-node launch args (argv after the program name).
    pub node_args: HashMap<NodeId, Vec<String>>,

    /// Canvas file nodes (in-memory or disk-backed) wired into apps.
    pub file_nodes: HashMap<NodeId, FileNode>,
    /// File connections as (file id, app node id).
    pub connections: Vec<(NodeId, NodeId)>,
    /// MIDI connections as (source node id, destination node id).
    pub midi_links: Vec<(NodeId, NodeId)>,
    /// HostPort nodes (canvas id -> localhost port).
    pub host_ports: HashMap<NodeId, u16>,
    /// Active servers: http node id -> (HostPort id, kill switch).
    pub serves: HashMap<NodeId, (NodeId, Arc<AtomicBool>)>,
    /// Network nodes (isolated virtual networks) by canvas id.
    pub net_nodes: HashSet<NodeId>,
    /// Which Network nodes are also Gateways (grant host-network access).
    pub gateways: HashSet<NodeId>,
    /// Network membership wires, as (app node id, Network node id).
    pub net_links: Vec<(NodeId, NodeId)>,

    /// Which workspace (tab) each node belongs to. Every workspace runs at once;
    /// the client filters this down to the tab it happens to be viewing.
    pub node_ws: HashMap<NodeId, NodeId>,
    /// The workspaces (tabs) in this document, in order — including empty ones.
    pub workspaces: Vec<NodeId>,

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
            available: doc.dependencies.clone(),
            workspace_path: path,
            win_pos: HashMap::new(),
            win_size: HashMap::new(),
            node_args: HashMap::new(),
            file_nodes: HashMap::new(),
            connections: Vec::new(),
            midi_links: Vec::new(),
            host_ports: HashMap::new(),
            serves: HashMap::new(),
            net_nodes: HashSet::new(),
            gateways: HashSet::new(),
            net_links: Vec::new(),
            node_ws: HashMap::new(),
            workspaces: doc.workspaces.iter().map(|w| w.id).collect(),
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
            let Some(dep) = self.available.iter().find(|d| d.name == n.name).cloned() else {
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
                    self.win_pos.insert(n.id, n.pos);
                    self.win_size.insert(n.id, n.size);
                    self.node_args.insert(n.id, args);
                }
                Err(e) => eprintln!("failed to restore {}: {e:#}", dep.name),
            }
        }

        // VirtualFile nodes: recreate empty shared buffers at their saved spots.
        for f in &saved.virtual_files {
            self.place(f.id, f.pos, f.size);
            if let Some(num) = f
                .name
                .strip_prefix("file")
                .and_then(|s| s.parse::<u32>().ok())
            {
                self.file_seq = self.file_seq.max(num);
            }
            self.file_nodes.insert(
                f.id,
                FileNode::Virtual(VirtualFile {
                    name: f.name.clone(),
                    data: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        }

        // HostMappedFile nodes: re-map their saved host paths (name = path).
        for f in &saved.host_files {
            self.place(f.id, f.pos, f.size);
            let path = PathBuf::from(&f.name);
            let name = host_file_name(&path);
            if let Some(num) = name
                .strip_prefix("host")
                .and_then(|s| s.parse::<u32>().ok())
            {
                self.host_seq = self.host_seq.max(num);
            }
            self.file_nodes
                .insert(f.id, FileNode::HostMapped(HostMappedFile { name, path }));
        }

        // Re-wire file connections: mount each file into its connected app's fs.
        for &(file_id, app_id) in &saved.connections {
            let (Some(file), Some(app)) = (self.file_nodes.get(&file_id), self.app_node(app_id))
            else {
                continue;
            };
            file.mount(&app.fs);
            self.connections.push((file_id, app_id));
        }

        // Re-wire MIDI connections through the router.
        for &(src, dst) in &saved.midi {
            let (Some(_), Some(dst_node)) = (self.app_node(src), self.app_node(dst)) else {
                continue;
            };
            self.host
                .midi()
                .lock()
                .unwrap()
                .connect(src, dst, dst_node.midi_in.clone());
            self.midi_links.push((src, dst));
        }

        // HostPort nodes: recreate at their saved positions and ports.
        for hp in &saved.host_ports {
            self.next_port = self.next_port.max(hp.port.saturating_add(1));
            self.place(hp.id, hp.pos, hp.size);
            self.host_ports.insert(hp.id, hp.port);
        }

        // Re-establish serve wiring (starts the servers again).
        for &(http_id, hostport_id) in &saved.serves {
            if self.app_node(http_id).is_some() && self.host_ports.contains_key(&hostport_id) {
                self.toggle_serve(http_id, hostport_id);
            }
        }

        // Network/Gateway nodes: recreate at their saved spots.
        for net in &saved.nets {
            self.place(net.id, net.pos, net.size);
            self.net_nodes.insert(net.id);
            if net.gateway {
                self.gateways.insert(net.id);
            }
        }
        // Re-wire network membership (rejoins the network + grants host access).
        for &(app_id, net_id) in &saved.net_links {
            if self.app_node(app_id).is_some() && self.net_nodes.contains(&net_id) {
                self.toggle_net(app_id, net_id);
            }
        }

        // Tag every node that got placed with the workspace it belongs to.
        let ids = saved
            .nodes
            .iter()
            .map(|n| n.id)
            .chain(saved.virtual_files.iter().map(|n| n.id))
            .chain(saved.host_files.iter().map(|n| n.id))
            .chain(saved.host_ports.iter().map(|p| p.id))
            .chain(saved.nets.iter().map(|n| n.id));
        for id in ids {
            if self.win_pos.contains_key(&id) {
                self.node_ws.insert(id, saved.id);
            }
        }
    }

    /// Record a node's canvas geometry.
    fn place(&mut self, id: NodeId, pos: [f32; 2], size: [f32; 2]) {
        self.win_pos.insert(id, pos);
        self.win_size.insert(id, size);
    }

    fn alloc_id(&mut self) -> NodeId {
        NodeId::new()
    }

    /// Every live canvas node id (app, file, port, network), for a client to
    /// reconcile its stacking order against.
    pub fn node_ids(&self) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = self.node_reg.lock().unwrap().iter().map(|n| n.id).collect();
        ids.extend(self.file_nodes.keys().copied());
        ids.extend(self.host_ports.keys().copied());
        ids.extend(self.net_nodes.iter().copied());
        ids
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
        self.place(id, pos, [360.0, 260.0]);
        self.node_args.insert(id, dep.args.clone());
        self.node_ws.insert(id, ws);
    }

    /// Create a new, empty in-memory VirtualFile node at `pos` in workspace `ws`.
    fn add_virtual_file(&mut self, pos: [f32; 2], ws: NodeId) {
        self.file_seq += 1;
        let id = self.alloc_id();
        self.place(id, pos, [FILE_W, FILE_H]);
        self.node_ws.insert(id, ws);
        self.file_nodes.insert(
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
        self.place(id, pos, [FILE_W, FILE_H]);
        self.node_ws.insert(id, ws);
        self.file_nodes
            .insert(id, FileNode::HostMapped(HostMappedFile { name, path }));
    }

    /// Create a HostPort node at `pos` (auto-assigned localhost port).
    fn add_host_port(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        let port = self.next_port;
        self.next_port = self.next_port.wrapping_add(1).max(8080);
        self.place(id, pos, [FILE_W, FILE_H]);
        self.node_ws.insert(id, ws);
        self.host_ports.insert(id, port);
    }

    /// Create a Network node at `pos`; returns its id.
    fn add_net_node(&mut self, pos: [f32; 2], ws: NodeId) -> NodeId {
        let id = self.alloc_id();
        self.place(id, pos, [FILE_W, FILE_H]);
        self.node_ws.insert(id, ws);
        self.net_nodes.insert(id);
        id
    }

    /// Create a Gateway node at `pos` (a Network whose members get host access).
    fn add_gateway_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.add_net_node(pos, ws);
        self.gateways.insert(id);
    }

    /// Register a new (empty) workspace tab with a client-minted id.
    fn add_workspace(&mut self, id: NodeId) {
        if !self.workspaces.contains(&id) {
            self.workspaces.push(id);
        }
    }

    /// Remove a node by kind (app/file/port/network).
    fn remove_any(&mut self, id: NodeId) {
        if self.file_nodes.contains_key(&id) {
            self.remove_file_node(id);
        } else if self.host_ports.contains_key(&id) {
            self.remove_host_port(id);
        } else if self.net_nodes.contains(&id) {
            self.remove_net_node(id);
        } else {
            self.close_node(id);
        }
    }

    /// Delete a workspace and every node in it. A no-op for the last workspace —
    /// a document always keeps at least one.
    fn remove_workspace(&mut self, id: NodeId) {
        if self.workspaces.len() <= 1 || !self.workspaces.contains(&id) {
            return;
        }
        let victims: Vec<NodeId> = self
            .node_ws
            .iter()
            .filter(|(_, &ws)| ws == id)
            .map(|(&n, _)| n)
            .collect();
        for n in victims {
            self.remove_any(n);
        }
        self.workspaces.retain(|&w| w != id);
    }

    /// (Re)run an idle or exited node's guest with its current args.
    fn run_node(&mut self, id: NodeId) {
        if let Some(node) = self.app_node(id) {
            let args = self.node_args.get(&id).cloned().unwrap_or_default();
            if let Err(e) = self.host.run_node(&node, &args) {
                eprintln!("failed to run {}: {e:#}", node.name);
            }
        }
    }

    /// Set a node's launch args from a whitespace-separated string.
    fn set_node_args(&mut self, id: NodeId, text: &str) {
        let args = text.split_whitespace().map(str::to_string).collect();
        self.node_args.insert(id, args);
    }

    /// Grant/revoke a node's host-network access (on its fabric stack).
    fn set_host_access(&self, app_id: NodeId, allow: bool) {
        if let Some(node) = self.app_node(app_id) {
            if let Some(stack) = node.net_stack() {
                stack.lock().unwrap().host_access = allow;
            }
        }
    }

    /// Toggle a connection between two nodes by their kinds: file⇄app mounts the
    /// file; http-app⇄HostPort serves on localhost; app⇄Network joins the network;
    /// app⇄app wires MIDI.
    fn connect_toggle(&mut self, a: NodeId, b: NodeId) {
        let af = self.file_nodes.contains_key(&a);
        let bf = self.file_nodes.contains_key(&b);
        let ap = self.host_ports.contains_key(&a);
        let bp = self.host_ports.contains_key(&b);
        let an = self.net_nodes.contains(&a);
        let bn = self.net_nodes.contains(&b);
        if af && !bf {
            self.toggle_file(a, b);
        } else if bf && !af {
            self.toggle_file(b, a);
        } else if ap && !bp {
            self.toggle_serve(b, a);
        } else if bp && !ap {
            self.toggle_serve(a, b);
        } else if an && !bn {
            self.toggle_net(b, a);
        } else if bn && !an {
            self.toggle_net(a, b);
        } else if !af && !bf && !ap && !bp && !an && !bn {
            self.toggle_midi(a, b);
        }
    }

    fn set_node_net(&self, app_id: NodeId, net: NodeId) {
        if let Some(node) = self.app_node(app_id) {
            if let Some(stack) = node.net_stack() {
                stack.lock().unwrap().net = net;
            }
        }
    }

    /// Wire (or unwire) app node `app_id` onto Network node `net_id`.
    fn toggle_net(&mut self, app_id: NodeId, net_id: NodeId) {
        if let Some(pos) = self
            .net_links
            .iter()
            .position(|&(a, n)| a == app_id && n == net_id)
        {
            self.net_links.remove(pos);
            self.set_node_net(app_id, app_id); // back to isolated
            self.set_host_access(app_id, false);
        } else {
            // One network per app: drop any existing membership first.
            self.net_links.retain(|&(a, _)| a != app_id);
            self.net_links.push((app_id, net_id));
            self.set_node_net(app_id, net_id);
            self.set_host_access(app_id, self.gateways.contains(&net_id));
        }
    }

    /// Ensure each wired node's fabric stack reflects its network membership.
    /// Nodes compile asynchronously, so one wired before its stack existed gets
    /// its membership applied here once it's ready.
    fn sync_net_membership(&self) {
        let nodes = self.node_reg.lock().unwrap().clone();
        for &(app, net) in &self.net_links {
            let Some(stack) = nodes
                .iter()
                .find(|n| n.id == app)
                .and_then(|n| n.net_stack())
            else {
                continue;
            };
            let host = self.gateways.contains(&net);
            let mut g = stack.lock().unwrap();
            if g.net != net || g.host_access != host {
                g.net = net;
                g.host_access = host;
            }
        }
    }

    /// Remove a Network/Gateway node, returning its members to isolation.
    fn remove_net_node(&mut self, id: NodeId) {
        self.net_nodes.remove(&id);
        self.gateways.remove(&id);
        let members: Vec<NodeId> = self
            .net_links
            .iter()
            .filter(|&&(_, n)| n == id)
            .map(|&(a, _)| a)
            .collect();
        for app in members {
            self.set_node_net(app, app);
            self.set_host_access(app, false);
        }
        self.net_links.retain(|&(_, n)| n != id);
        self.forget(id);
    }

    /// Wire (or unwire) a wasi:http node to a HostPort: start/stop serving it.
    fn toggle_serve(&mut self, http_id: NodeId, hostport_id: NodeId) {
        if let Some((_, kill)) = self.serves.remove(&http_id) {
            kill.store(true, Ordering::Relaxed);
            return;
        }
        let Some(node) = self.app_node(http_id) else {
            return;
        };
        let Some(path) = node.http_path() else {
            return; // not a wasi:http server node
        };
        let Some(&port) = self.host_ports.get(&hostport_id) else {
            return;
        };
        let kill = Arc::new(AtomicBool::new(false));
        if let Err(e) = self
            .host
            .serve(&path, port, Some(node.term_io.clone()), kill.clone())
        {
            eprintln!("failed to serve {} on :{port}: {e:#}", node.name);
            return;
        }
        self.serves.insert(http_id, (hostport_id, kill));
    }

    /// Remove a HostPort node, stopping any server bound through it.
    fn remove_host_port(&mut self, id: NodeId) {
        self.host_ports.remove(&id);
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
        self.forget(id);
    }

    /// Change a HostPort's localhost port by `delta`, live-rebinding any server.
    fn change_port(&mut self, id: NodeId, delta: i32) {
        let Some(&cur) = self.host_ports.get(&id) else {
            return;
        };
        let new = (cur as i32 + delta).clamp(1, 65535) as u16;
        if new == cur {
            return;
        }
        self.host_ports.insert(id, new);
        self.next_port = self.next_port.max(new.saturating_add(1));
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
            self.toggle_serve(http, id);
        }
    }

    /// Wire (or unwire) file node `file_id` into app node `app_id`'s filesystem.
    fn toggle_file(&mut self, file_id: NodeId, app_id: NodeId) {
        let Some(app) = self.app_node(app_id) else {
            return;
        };
        let file = &self.file_nodes[&file_id];
        if let Some(pos) = self
            .connections
            .iter()
            .position(|&(f, a)| f == file_id && a == app_id)
        {
            crate::vfs::unmount_file(&app.fs, file.name());
            self.connections.remove(pos);
        } else {
            file.mount(&app.fs);
            self.connections.push((file_id, app_id));
        }
    }

    /// Wire (or unwire) app node `src`'s MIDI output into app node `dst`'s input.
    fn toggle_midi(&mut self, src: NodeId, dst: NodeId) {
        let (Some(_src), Some(dst_node)) = (self.app_node(src), self.app_node(dst)) else {
            return;
        };
        let router = self.host.midi();
        let mut routes = router.lock().unwrap();
        if let Some(pos) = self
            .midi_links
            .iter()
            .position(|&(s, d)| s == src && d == dst)
        {
            routes.disconnect(src, dst);
            self.midi_links.remove(pos);
        } else {
            routes.connect(src, dst, dst_node.midi_in.clone());
            self.midi_links.push((src, dst));
        }
    }

    /// Remove a file node, unmounting it from every app it was connected to.
    fn remove_file_node(&mut self, id: NodeId) {
        let Some(file) = self.file_nodes.remove(&id) else {
            return;
        };
        let nodes = self.node_reg.lock().unwrap().clone();
        for &(f, a) in self.connections.iter().filter(|&&(f, _)| f == id) {
            let _ = f;
            if let Some(app) = nodes.iter().find(|n| n.id == a) {
                crate::vfs::unmount_file(&app.fs, file.name());
            }
        }
        self.connections.retain(|&(f, _)| f != id);
        self.forget(id);
    }

    /// Drop a removed node's canvas geometry.
    fn forget(&mut self, id: NodeId) {
        self.win_pos.remove(&id);
        self.win_size.remove(&id);
        self.node_ws.remove(&id);
    }

    /// Whether the given wire still connects two live nodes.
    pub fn wire_exists(&self, w: Wire) -> bool {
        match w {
            Wire::File(f, a) => self.connections.contains(&(f, a)),
            Wire::Midi(s, d) => self.midi_links.contains(&(s, d)),
            Wire::Serve(h, hp) => self.serves.get(&h).map(|(p, _)| *p) == Some(hp),
            Wire::Net(app, net) => self.net_links.contains(&(app, net)),
        }
    }

    /// Remove the given connection (the same effect as toggling it off).
    fn disconnect_wire(&mut self, w: Wire) {
        match w {
            Wire::File(f, a) => {
                if self.connections.contains(&(f, a)) {
                    self.toggle_file(f, a);
                }
            }
            Wire::Midi(s, d) => {
                if self.midi_links.contains(&(s, d)) {
                    self.toggle_midi(s, d);
                }
            }
            Wire::Serve(h, hp) => {
                if self.serves.contains_key(&h) {
                    self.toggle_serve(h, hp);
                }
            }
            Wire::Net(app, net) => {
                if self.net_links.contains(&(app, net)) {
                    self.toggle_net(app, net);
                }
            }
        }
    }

    /// Move / resize a node.
    fn set_node_pos(&mut self, id: NodeId, pos: [f32; 2]) {
        self.win_pos.insert(id, pos);
    }
    fn set_node_size(&mut self, id: NodeId, size: [f32; 2]) {
        self.win_size.insert(id, size);
    }

    /// One server step: reconcile any wiring that was pending on a still-loading
    /// node. Cheap; a client calls it each frame, headless in its tick loop.
    pub fn tick(&mut self) {
        self.sync_net_membership();
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
        self.connections.retain(|&(_, app)| app != id);
        self.net_links.retain(|&(app, _)| app != id);
        self.host.midi().lock().unwrap().remove_node(id);
        self.midi_links.retain(|&(s, d)| s != id && d != id);
        if let Some((_, kill)) = self.serves.remove(&id) {
            kill.store(true, Ordering::Relaxed);
        }
        self.node_args.remove(&id);
        self.forget(id);
    }

    /// Snapshot every workspace into a [`Document`] and write it back to disk.
    pub fn save(&self) {
        let node_list = self.node_reg.lock().unwrap().clone();
        let workspaces = self
            .workspaces
            .iter()
            .map(|&ws_id| {
                let mine = |id: &NodeId| self.node_ws.get(id).copied() == Some(ws_id);
                Workspace {
                    id: ws_id,
                    nodes: node_list
                        .iter()
                        .filter(|n| mine(&n.id))
                        .filter_map(|node| {
                            Some(NodeState {
                                name: node.name.clone(),
                                id: node.id,
                                pos: *self.win_pos.get(&node.id)?,
                                size: *self.win_size.get(&node.id)?,
                                options: node.options.lock().unwrap().clone(),
                                args: self.node_args.get(&node.id).cloned().unwrap_or_default(),
                            })
                        })
                        .collect(),
                    virtual_files: self
                        .file_nodes
                        .iter()
                        .filter(|(id, _)| mine(id))
                        .filter_map(|(&id, f)| match f {
                            FileNode::Virtual(v) => Some(NodeState {
                                name: v.name.clone(),
                                id,
                                pos: *self.win_pos.get(&id)?,
                                size: *self.win_size.get(&id)?,
                                options: Vec::new(),
                                args: Vec::new(),
                            }),
                            FileNode::HostMapped(_) => None,
                        })
                        .collect(),
                    host_files: self
                        .file_nodes
                        .iter()
                        .filter(|(id, _)| mine(id))
                        .filter_map(|(&id, f)| match f {
                            FileNode::HostMapped(h) => Some(NodeState {
                                name: h.path.to_string_lossy().into_owned(),
                                id,
                                pos: *self.win_pos.get(&id)?,
                                size: *self.win_size.get(&id)?,
                                options: Vec::new(),
                                args: Vec::new(),
                            }),
                            FileNode::Virtual(_) => None,
                        })
                        .collect(),
                    host_ports: self
                        .host_ports
                        .iter()
                        .filter(|(id, _)| mine(id))
                        .filter_map(|(&id, &port)| {
                            Some(PortState {
                                id,
                                port,
                                pos: *self.win_pos.get(&id)?,
                                size: *self.win_size.get(&id)?,
                            })
                        })
                        .collect(),
                    connections: self
                        .connections
                        .iter()
                        .filter(|(f, _)| mine(f))
                        .copied()
                        .collect(),
                    midi: self
                        .midi_links
                        .iter()
                        .filter(|(s, _)| mine(s))
                        .copied()
                        .collect(),
                    serves: self
                        .serves
                        .iter()
                        .filter(|(http, _)| mine(http))
                        .map(|(&http, &(hostport, _))| (http, hostport))
                        .collect(),
                    nets: self
                        .net_nodes
                        .iter()
                        .filter(|id| mine(id))
                        .filter_map(|&id| {
                            Some(NetState {
                                id,
                                gateway: self.gateways.contains(&id),
                                pos: *self.win_pos.get(&id)?,
                                size: *self.win_size.get(&id)?,
                            })
                        })
                        .collect(),
                    net_links: self
                        .net_links
                        .iter()
                        .filter(|(a, _)| mine(a))
                        .copied()
                        .collect(),
                }
            })
            .collect();
        let doc = Document {
            dependencies: self.available.clone(),
            workspaces,
        };
        if let Err(e) = doc.save(&self.workspace_path) {
            eprintln!("failed to save workspace: {e}");
        }
    }

    /// Apply a client [`Command`]. The single entry point for mutations — the
    /// same one a networked client's messages would flow through.
    pub fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::Launch { dep, pos, ws } => {
                if let Some(dep) = self.available.get(dep).cloned() {
                    self.launch(&dep, pos, ws);
                }
            }
            Command::AddVirtualFile { pos, ws } => self.add_virtual_file(pos, ws),
            Command::AddHostFile { pos, ws } => self.add_host_mapped_file(pos, ws),
            Command::AddPort { pos, ws } => self.add_host_port(pos, ws),
            Command::AddNetwork { pos, ws } => {
                self.add_net_node(pos, ws);
            }
            Command::AddGateway { pos, ws } => self.add_gateway_node(pos, ws),
            Command::AddWorkspace { id } => self.add_workspace(id),
            Command::RemoveWorkspace { id } => self.remove_workspace(id),
            Command::RemoveNode { id } => self.remove_any(id),
            Command::MoveNode { id, pos } => self.set_node_pos(id, pos),
            Command::ResizeNode { id, size } => self.set_node_size(id, size),
            Command::Connect { a, b } => self.connect_toggle(a, b),
            Command::Disconnect { wire } => self.disconnect_wire(wire),
            Command::RunNode { id } => self.run_node(id),
            Command::SetNodeArgs { id, args } => self.set_node_args(id, &args),
            Command::ChangePort { id, delta } => self.change_port(id, delta),
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
        let serves = self
            .serves
            .iter()
            .map(|(&http, &(hostport, _))| (http, hostport))
            .collect();
        View {
            node_ids: self.node_ids(),
            win_pos: self.win_pos.clone(),
            win_size: self.win_size.clone(),
            file_nodes,
            host_ports: self.host_ports.clone(),
            net_nodes: self.net_nodes.clone(),
            gateways: self.gateways.clone(),
            connections: self.connections.clone(),
            midi_links: self.midi_links.clone(),
            net_links: self.net_links.clone(),
            serves,
            node_args: self.node_args.clone(),
            available: self.available.clone(),
            nodes,
            surfaces,
            node_ws: self.node_ws.clone(),
            workspaces: self.workspaces.clone(),
        }
    }
}
