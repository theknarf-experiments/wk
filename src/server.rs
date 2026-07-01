//! The wk **server**: the authoritative half of a running workspace. It owns the
//! workspace file, the wasm runtime (`PluginHost` + the fabric + MIDI), and the
//! *document* — every canvas node (app/file/port/network), where each sits, and
//! all the wiring between them. Clients (the GUI window, a headless runner, a
//! test harness, an MCP server, a network peer) drive it: they issue mutations
//! and read its state to render. In single-player the client just holds the
//! `Server` and calls it directly; the same surface is what a networked client
//! would send over a socket.
//!
//! Camera/selection/palette/drag live in the *client*, not here. Node positions
//! and sizes are the server's because they're shared across clients and saved to
//! the workspace file.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::plugin::{NodeRegistry, PluginHost, SharedNode, SurfaceRegistry};
use crate::workspace::{Dependency, NetState, NodeState, PortState, Workspace};

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
    /// The real path on the host.
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
    /// Persisted camera (pan x, pan y, zoom) — the client reads it as its initial
    /// view and writes its current view back on save.
    pub camera: (f32, f32, f32),

    /// Node positions/sizes, keyed by node id (shared canvas geometry).
    pub win_pos: HashMap<u64, [f32; 2]>,
    pub win_size: HashMap<u64, [f32; 2]>,
    /// Per-node launch args (argv after the program name).
    pub node_args: HashMap<u64, Vec<String>>,

    /// Canvas file nodes (in-memory or disk-backed) wired into apps.
    pub file_nodes: HashMap<u64, FileNode>,
    /// File connections as (file id, app node id).
    pub connections: Vec<(u64, u64)>,
    /// MIDI connections as (source node id, destination node id).
    pub midi_links: Vec<(u64, u64)>,
    /// HostPort nodes (canvas id -> localhost port).
    pub host_ports: HashMap<u64, u16>,
    /// Active servers: http node id -> (HostPort id, kill switch).
    pub serves: HashMap<u64, (u64, Arc<AtomicBool>)>,
    /// Network nodes (isolated virtual networks) by canvas id.
    pub net_nodes: HashSet<u64>,
    /// Which Network nodes are also Gateways (grant host-network access).
    pub gateways: HashSet<u64>,
    /// Network membership wires, as (app node id, Network node id).
    pub net_links: Vec<(u64, u64)>,

    next_node_id: u64,
    next_port: u16,
    file_seq: u32,
    host_seq: u32,
}

impl Server {
    /// Create a server and instantiate the given workspace (spawn its nodes and
    /// re-apply its wiring). `path` is the `.wk` file to save back to.
    pub fn new(ws: &Workspace, path: PathBuf) -> Result<Self, String> {
        let host = PluginHost::new().map_err(|e| format!("{e:#}"))?;
        let mut server = Server {
            host,
            registry: Arc::new(Mutex::new(Vec::new())),
            node_reg: Arc::new(Mutex::new(Vec::new())),
            available: ws.dependencies.clone(),
            workspace_path: path,
            camera: ws.camera,
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
            next_node_id: 0,
            next_port: 8080,
            file_seq: 0,
            host_seq: 0,
        };
        server.instantiate(ws);
        Ok(server)
    }

    /// Spawn the workspace's nodes and re-apply its wiring (used at load). Node
    /// positions are set here so every node has a place the moment it exists.
    fn instantiate(&mut self, saved: &Workspace) {
        let mut max_id = 0;

        // App nodes: resolve the dependency by name, spawn with the saved id.
        for n in &saved.nodes {
            max_id = max_id.max(n.id);
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
            max_id = max_id.max(f.id);
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
            max_id = max_id.max(f.id);
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
            max_id = max_id.max(hp.id);
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
            max_id = max_id.max(net.id);
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

        self.next_node_id = max_id + 1;
    }

    /// Record a node's canvas geometry.
    fn place(&mut self, id: u64, pos: [f32; 2], size: [f32; 2]) {
        self.win_pos.insert(id, pos);
        self.win_size.insert(id, size);
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_node_id;
        self.next_node_id += 1;
        id
    }

    /// Every live canvas node id (app, file, port, network), for a client to
    /// reconcile its stacking order against.
    pub fn node_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.node_reg.lock().unwrap().iter().map(|n| n.id).collect();
        ids.extend(self.file_nodes.keys().copied());
        ids.extend(self.host_ports.keys().copied());
        ids.extend(self.net_nodes.iter().copied());
        ids
    }

    /// The live app node with id `id`, if it is an app (not a file) node.
    pub fn app_node(&self, id: u64) -> Option<SharedNode> {
        self.node_reg
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
    }

    // ---- node creation (positions come from the client's view) ----

    /// Launch a dependency as a new app node at `pos`.
    pub fn launch(&mut self, dep: &Dependency, pos: [f32; 2]) {
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
    }

    /// Create a new, empty in-memory VirtualFile node at `pos`.
    pub fn add_virtual_file(&mut self, pos: [f32; 2]) {
        self.file_seq += 1;
        let id = self.alloc_id();
        self.place(id, pos, [FILE_W, FILE_H]);
        self.file_nodes.insert(
            id,
            FileNode::Virtual(VirtualFile {
                name: format!("file{}", self.file_seq),
                data: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    }

    /// Create a HostMappedFile node backed by a fresh host file (`host<n>`).
    pub fn add_host_mapped_file(&mut self, pos: [f32; 2]) {
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
        self.file_nodes
            .insert(id, FileNode::HostMapped(HostMappedFile { name, path }));
    }

    /// Create a HostPort node at `pos` (auto-assigned localhost port).
    pub fn add_host_port(&mut self, pos: [f32; 2]) {
        let id = self.alloc_id();
        let port = self.next_port;
        self.next_port = self.next_port.wrapping_add(1).max(8080);
        self.place(id, pos, [FILE_W, FILE_H]);
        self.host_ports.insert(id, port);
    }

    /// Create a Network node at `pos`; returns its id.
    pub fn add_net_node(&mut self, pos: [f32; 2]) -> u64 {
        let id = self.alloc_id();
        self.place(id, pos, [FILE_W, FILE_H]);
        self.net_nodes.insert(id);
        id
    }

    /// Create a Gateway node at `pos` (a Network whose members get host access).
    pub fn add_gateway_node(&mut self, pos: [f32; 2]) {
        let id = self.add_net_node(pos);
        self.gateways.insert(id);
    }

    // ---- running / args ----

    /// (Re)run an idle or exited node's guest with its current args.
    pub fn run_node(&mut self, id: u64) {
        if let Some(node) = self.app_node(id) {
            let args = self.node_args.get(&id).cloned().unwrap_or_default();
            if let Err(e) = self.host.run_node(&node, &args) {
                eprintln!("failed to run {}: {e:#}", node.name);
            }
        }
    }

    /// Set a node's launch args from a whitespace-separated string.
    pub fn set_node_args(&mut self, id: u64, text: &str) {
        let args = text.split_whitespace().map(str::to_string).collect();
        self.node_args.insert(id, args);
    }

    // ---- wiring ----

    /// Grant/revoke a node's host-network access (on its fabric stack).
    fn set_host_access(&self, app_id: u64, allow: bool) {
        if let Some(node) = self.app_node(app_id) {
            if let Some(stack) = node.net_stack() {
                stack.lock().unwrap().host_access = allow;
            }
        }
    }

    /// Toggle a connection between two nodes by their kinds: file⇄app mounts the
    /// file; http-app⇄HostPort serves on localhost; app⇄Network joins the network;
    /// app⇄app wires MIDI.
    pub fn connect_toggle(&mut self, a: u64, b: u64) {
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

    fn set_node_net(&self, app_id: u64, net: u64) {
        if let Some(node) = self.app_node(app_id) {
            if let Some(stack) = node.net_stack() {
                stack.lock().unwrap().net = net;
            }
        }
    }

    /// Wire (or unwire) app node `app_id` onto Network node `net_id`.
    fn toggle_net(&mut self, app_id: u64, net_id: u64) {
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
    pub fn remove_net_node(&mut self, id: u64) {
        self.net_nodes.remove(&id);
        self.gateways.remove(&id);
        let members: Vec<u64> = self
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
    fn toggle_serve(&mut self, http_id: u64, hostport_id: u64) {
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
    pub fn remove_host_port(&mut self, id: u64) {
        self.host_ports.remove(&id);
        let bound: Vec<u64> = self
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
    pub fn change_port(&mut self, id: u64, delta: i32) {
        let Some(&cur) = self.host_ports.get(&id) else {
            return;
        };
        let new = (cur as i32 + delta).clamp(1, 65535) as u16;
        if new == cur {
            return;
        }
        self.host_ports.insert(id, new);
        self.next_port = self.next_port.max(new.saturating_add(1));
        let bound: Vec<u64> = self
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
    fn toggle_file(&mut self, file_id: u64, app_id: u64) {
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
    fn toggle_midi(&mut self, src: u64, dst: u64) {
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
    pub fn remove_file_node(&mut self, id: u64) {
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
    fn forget(&mut self, id: u64) {
        self.win_pos.remove(&id);
        self.win_size.remove(&id);
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
    pub fn disconnect_wire(&mut self, w: Wire) {
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
    pub fn set_node_pos(&mut self, id: u64, pos: [f32; 2]) {
        self.win_pos.insert(id, pos);
    }
    pub fn set_node_size(&mut self, id: u64, size: [f32; 2]) {
        self.win_size.insert(id, size);
    }

    // ---- lifecycle ----

    /// One server step: reconcile any wiring that was pending on a still-loading
    /// node. Cheap; a client calls it each frame, headless in its tick loop.
    pub fn tick(&mut self) {
        self.sync_net_membership();
    }

    /// Kill a node and drop everything referencing it (its wiring, geometry, and
    /// the wasm instance). Used when a client closes a node.
    pub fn close_node(&mut self, id: u64) {
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

    /// Snapshot the document into a [`Workspace`] and write it back to disk.
    /// `camera` is the client's current view (persisted for next open).
    pub fn save(&self, camera: (f32, f32, f32)) {
        let nodes = self
            .node_reg
            .lock()
            .unwrap()
            .iter()
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
            .collect();
        let virtual_files = self
            .file_nodes
            .iter()
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
            .collect();
        let host_files = self
            .file_nodes
            .iter()
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
            .collect();
        let host_ports = self
            .host_ports
            .iter()
            .filter_map(|(&id, &port)| {
                Some(PortState {
                    id,
                    port,
                    pos: *self.win_pos.get(&id)?,
                    size: *self.win_size.get(&id)?,
                })
            })
            .collect();
        let serves = self
            .serves
            .iter()
            .map(|(&http, &(hostport, _))| (http, hostport))
            .collect();
        let nets = self
            .net_nodes
            .iter()
            .filter_map(|&id| {
                Some(NetState {
                    id,
                    gateway: self.gateways.contains(&id),
                    pos: *self.win_pos.get(&id)?,
                    size: *self.win_size.get(&id)?,
                })
            })
            .collect();
        let ws = Workspace {
            dependencies: self.available.clone(),
            camera,
            nodes,
            virtual_files,
            host_files,
            host_ports,
            connections: self.connections.clone(),
            midi: self.midi_links.clone(),
            serves,
            nets,
            net_links: self.net_links.clone(),
        };
        if let Err(e) = ws.save(&self.workspace_path) {
            eprintln!("failed to save workspace: {e}");
        }
    }
}

/// A connection wire, identified by the two node ids it joins (by kind).
#[derive(Clone, Copy, PartialEq)]
pub enum Wire {
    File(u64, u64),
    Midi(u64, u64),
    Serve(u64, u64),
    Net(u64, u64),
}
