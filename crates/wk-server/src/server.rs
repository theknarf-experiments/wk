//! The wk **server**: the authoritative half of a running workspace. It owns the
//! workspace file, the wasm runtime (`PluginHost` + the fabric + MIDI), and the
//! *document* — every canvas node (app/file/port/network), where each sits, and
//! all the wiring between them. Clients drive it through a `ServerHandle`: they
//! issue mutations and read its state to render.
//!
//! Camera/selection/palette/drag live in the *client*, not here. Node positions
//! and sizes are the server's because they're shared across clients and saved to
//! the workspace file.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::plugin::{NodeRegistry, PluginHost, SharedNode, SharedSurface, SurfaceRegistry};
use crate::wiring::{self, NodeClass};
use crate::workspace::{
    secret_bytes, secret_hex, Dependency, Document, NodeSnap, SnapKind, Workspace,
};
use wk_protocol::{
    BoundaryWire, Command, NodeId, NodeKind, PortDir, PortKind, Resource, ResourceRef, ViewMode,
    Wire,
};

/// Default canvas size of a file / port / network node, in canvas pixels.
pub const FILE_W: f32 = 130.0;
pub const FILE_H: f32 = 44.0;
/// Default size of a new note node.
pub const NOTE_W: f32 = 220.0;
pub const NOTE_H: f32 = 130.0;

/// An in-memory canvas file node: a named shared buffer you wire into app nodes.
pub struct Volume {
    pub name: String,
    pub data: crate::vfs::SharedFile,
    /// When set, the bytes are saved to a sidecar beside the `.wk` file and
    /// restored on load; otherwise the volume is ephemeral (empty each run).
    pub persist: bool,
}

/// A canvas file node backed by a real host path (a file or a folder).
pub struct BindMount {
    /// Default in-app mount name (the path's base name); a bind may override the
    /// mount point per connection (see `Graph.mount_paths`).
    pub name: String,
    pub path: PathBuf,
}

/// A canvas volume node, bind-mounted into app nodes (at a per-connection path).
pub enum FileNode {
    /// An in-memory named volume.
    Volume(Volume),
    /// A host-path bind mount.
    Bind(BindMount),
}

impl FileNode {
    /// The in-app file name this node mounts as.
    pub fn name(&self) -> &str {
        match self {
            FileNode::Volume(f) => &f.name,
            FileNode::Bind(f) => &f.name,
        }
    }

    /// Current size in bytes (in-memory length, or the host file's size).
    pub fn size(&self) -> usize {
        match self {
            FileNode::Volume(f) => f.data.lock().unwrap().len(),
            FileNode::Bind(f) => std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0) as usize,
        }
    }

    /// Bind this volume into app filesystem `fs` at the path `at` (by kind). A
    /// BindMount pointing at a host directory mirrors the whole tree.
    pub fn mount(&self, fs: &crate::vfs::SharedFs, at: &str, writable: bool) {
        match self {
            FileNode::Volume(f) => crate::vfs::mount_file(fs, at, f.data.clone(), writable),
            FileNode::Bind(f) => crate::vfs::mount_host(fs, at, f.path.clone(), writable),
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
    /// A BindMount whose host path is a directory (mirrored as a tree).
    pub is_dir: bool,
    /// A Volume with persistence turned on (bytes saved to a sidecar).
    pub persist: bool,
}

/// Which p2p transport an uplink node tunnels over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UplinkKind {
    Iroh,
    Veilid,
}

impl UplinkKind {
    /// The display label shown on the canvas node.
    pub fn label(self) -> &'static str {
        match self {
            UplinkKind::Iroh => "Iroh",
            UplinkKind::Veilid => "Veilid",
        }
    }
}

/// Render-facing metadata about an uplink node (Iroh or Veilid).
#[derive(Clone)]
pub struct UplinkMeta {
    pub kind: UplinkKind,
    /// This uplink's dialable ticket (shown so the user can share it).
    pub ticket: String,
    /// Live tunnel connections.
    pub peers: usize,
}

/// A running uplink of either transport, with one surface for the server.
enum UplinkHandle {
    Iroh(wk_fabric::uplink::Uplink),
    Veilid(wk_fabric::veilid::VeilidUplink),
}

impl UplinkHandle {
    fn kind(&self) -> UplinkKind {
        match self {
            UplinkHandle::Iroh(_) => UplinkKind::Iroh,
            UplinkHandle::Veilid(_) => UplinkKind::Veilid,
        }
    }
    fn ticket(&self) -> &str {
        match self {
            UplinkHandle::Iroh(u) => u.ticket(),
            UplinkHandle::Veilid(u) => u.ticket(),
        }
    }
    fn dial(&self, ticket: &str) -> wasmtime::anyhow::Result<()> {
        match self {
            UplinkHandle::Iroh(u) => u.dial(ticket),
            UplinkHandle::Veilid(u) => u.dial(ticket),
        }
    }
    fn set_net(&self, net: NodeId) {
        match self {
            UplinkHandle::Iroh(u) => u.set_net(net),
            UplinkHandle::Veilid(u) => u.set_net(net),
        }
    }
    fn peers(&self) -> usize {
        match self {
            UplinkHandle::Iroh(u) => u.peers(),
            UplinkHandle::Veilid(u) => u.peers(),
        }
    }
}

/// A read-only snapshot of the document a client renders from. Produced by
/// [`Server::view`] under one lock; everything is owned/cloned except the live
/// surface and node handles, which are `Arc`s a client uses to paint pixels and
/// forward input (the in-process fast path; a networked client would receive
/// pixel streams instead).
#[derive(Clone, Default)]
pub struct View {
    /// Every canvas node id (app/file/port/network), for draw-order reconcile.
    pub node_ids: Vec<NodeId>,
    pub win_pos: HashMap<NodeId, [f32; 2]>,
    pub win_size: HashMap<NodeId, [f32; 2]>,
    /// Free 3D poses (`[x, y, z, yaw]`) for nodes placed off the layout
    /// cylinder in the 3D world.
    pub pos3d: HashMap<NodeId, [f32; 4]>,
    /// Nodes asking for no flat panel in the 3D world — they are drawn as
    /// their `wk:scene` objects alone.
    pub hidden_panel3d: HashSet<NodeId>,
    /// Every live wk:scene entity (plugin-owned 3D objects), across all nodes.
    /// The surrounding world is in here too — a node publishing its plaza as
    /// scenery, not a special case in the view.
    pub scene_entities: Vec<crate::scene::SharedEntity>,
    /// The last `wk view` request and its sequence number. A client applies it
    /// when the sequence advances past the one it last saw, so a request lands
    /// once per client and a client attaching later isn't yanked by an old one.
    pub view_mode: (u64, ViewMode),
    pub file_nodes: HashMap<NodeId, FileMeta>,
    pub host_ports: HashMap<NodeId, u16>,
    /// Note nodes (canvas id -> the sticky-note text).
    pub notes: HashMap<NodeId, String>,
    /// MidiIn nodes (canvas id -> the resolved/target device name), for the UI
    /// to label them.
    pub midi_ins: HashMap<NodeId, String>,
    /// MidiOut nodes (canvas id -> the resolved/target device name), for the UI
    /// to label them.
    pub midi_outs: HashMap<NodeId, String>,
    /// HostService nodes (canvas id -> fabric name + host target), for the UI
    /// to label and edit them.
    pub host_services: HashMap<NodeId, HostService>,
    /// Boundary-port nodes (canvas id -> its declaration), for the UI to draw
    /// and to give the right typed dot.
    pub boundary_ports: HashMap<NodeId, BoundaryPort>,
    /// `group` nodes (canvas id -> what the instance is), for the UI to draw an
    /// instance with the definition's own edges on it.
    pub groups: HashMap<NodeId, GroupInfo>,
    pub net_nodes: HashSet<NodeId>,
    /// Router nodes: wired to two or more Networks, they bridge them.
    pub routers: HashSet<NodeId>,
    /// What to call each app node on a canvas: the name someone *chose* for
    /// it, else its type. A generated name is a handle, not a description —
    /// it tells a human nothing a card's position doesn't already say — while
    /// a chosen one is the whole reason someone chose it.
    pub node_labels: HashMap<NodeId, String>,
    pub gateways: HashSet<NodeId>,
    pub uplinks: HashMap<NodeId, UplinkMeta>,
    pub connections: Vec<(NodeId, NodeId)>,
    /// Per-bind mount-path overrides as (volume, app) → in-app path. Absent = the
    /// default (the volume's name at the root); the UI shows/edits these.
    pub mount_paths: HashMap<(NodeId, NodeId), String>,
    /// App nodes whose component serves a filesystem (imports
    /// `wk:fs/provider`) — mount sources the UI wires into apps like volumes.
    pub fs_providers: HashSet<NodeId>,
    pub midi_links: Vec<(NodeId, NodeId)>,
    pub net_links: Vec<(NodeId, NodeId)>,
    /// Screen-capture grants as (app, Capture node).
    pub capture_links: Vec<(NodeId, NodeId)>,
    /// Host-clipboard grants as (app, Clipboard node).
    pub clipboard_links: Vec<(NodeId, NodeId)>,
    /// API grants as (app, Api node).
    pub api_links: Vec<(NodeId, NodeId)>,
    /// Per-serve container-port overrides as (served, hostport) → guest port, so
    /// the UI can show a HostPort's `host→container` mapping.
    pub serve_ports: HashMap<(NodeId, NodeId), u16>,
    /// Nodes a CLI client has attached to — the UI treats these as detached
    /// (it stops draining/feeding their terminal).
    pub attached: std::collections::HashSet<NodeId>,
    /// Each Capture node's frame slot — the local client writes captured
    /// canvas frames into these (only while the node has a wired app).
    pub capture_feeds: HashMap<NodeId, crate::capture::SharedFrameSlot>,
    /// Each Clipboard node's board — the local client pumps the host's real
    /// system clipboard through these (it owns the only `arboard` handle;
    /// wk-server never touches a platform clipboard API).
    pub clipboard_boards: HashMap<NodeId, crate::clipboard::SharedBoard>,
    /// Api nodes on the canvas (wk's client API as a capability source).
    pub api_nodes: HashSet<NodeId>,
    /// http node id -> HostPort node id.
    pub serves: HashMap<NodeId, NodeId>,
    /// HostPort node id -> a bind-failure message (localhost port unavailable),
    /// for the client to surface as a warning.
    pub port_errors: HashMap<NodeId, String>,
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
    /// Workspace names keyed by workspace id — what the tab bar labels a tab
    /// with. Only named tabs have an entry.
    pub workspace_names: HashMap<NodeId, String>,
}

/// Bridge one accepted fabric connection to the host service at `target`:
/// dial it, then splice bytes both ways until either side closes. Each
/// direction gets its own thread (blocking `io::copy`), with a half-close
/// (`shutdown(Write)`) propagating EOF so protocols that close one way first
/// — an HTTP client done sending, a WebSocket FIN — behave as they would on a
/// real network. A dead or refusing target just drops the fabric connection,
/// which the guest sees as ECONNRESET.
fn bridge_to_host(stream: std::os::unix::net::UnixStream, target: String) {
    std::thread::Builder::new()
        .name("wk-hostsvc-bridge".into())
        .spawn(move || {
            use std::net::{Shutdown, TcpStream};
            let Ok(host) = TcpStream::connect_timeout(
                &match target.parse() {
                    Ok(addr) => addr,
                    Err(_) => {
                        // Not a literal addr:port — resolve it (allows
                        // `localhost:8080` and LAN hostnames).
                        use std::net::ToSocketAddrs;
                        match target.to_socket_addrs().ok().and_then(|mut a| a.next()) {
                            Some(addr) => addr,
                            None => return,
                        }
                    }
                },
                std::time::Duration::from_secs(10),
            ) else {
                return;
            };
            let _ = host.set_nodelay(true);
            let (Ok(mut host_rd), Ok(mut fab_rd)) = (host.try_clone(), stream.try_clone()) else {
                return;
            };
            let mut host_wr = host;
            let mut fab_wr = stream;
            let up = std::thread::spawn(move || {
                let _ = std::io::copy(&mut fab_rd, &mut host_wr);
                let _ = host_wr.shutdown(Shutdown::Write);
            });
            let _ = std::io::copy(&mut host_rd, &mut fab_wr);
            let _ = fab_wr.shutdown(Shutdown::Write);
            let _ = up.join();
        })
        .expect("spawn hostsvc bridge thread");
}

/// Keep the entries of an id-keyed map whose key satisfies `keep`.
fn keep_map<V: Clone>(
    m: &HashMap<NodeId, V>,
    keep: impl Fn(&NodeId) -> bool,
) -> HashMap<NodeId, V> {
    m.iter()
        .filter(|(id, _)| keep(id))
        .map(|(&k, v)| (k, v.clone()))
        .collect()
}

/// Keep the members of an id set that satisfy `keep`.
fn keep_set(s: &HashSet<NodeId>, keep: impl Fn(&NodeId) -> bool) -> HashSet<NodeId> {
    s.iter().copied().filter(|id| keep(id)).collect()
}

/// Keep the wires of a `(source, dest)` list whose *source* satisfies `keep`.
fn keep_pairs(v: &[(NodeId, NodeId)], keep: impl Fn(&NodeId) -> bool) -> Vec<(NodeId, NodeId)> {
    v.iter().copied().filter(|(a, _)| keep(a)).collect()
}

/// Keep the entries of a `(source, dest)`-keyed map whose *source* satisfies
/// `keep` (per-wire overrides: mount paths, container ports).
fn keep_pair_map<V: Clone>(
    m: &HashMap<(NodeId, NodeId), V>,
    keep: impl Fn(&NodeId) -> bool,
) -> HashMap<(NodeId, NodeId), V> {
    m.iter()
        .filter(|((a, _), _)| keep(a))
        .map(|(&k, v)| (k, v.clone()))
        .collect()
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
        View {
            node_ids: self.node_ids.iter().copied().filter(mine).collect(),
            win_pos: keep_map(&self.win_pos, mine),
            win_size: keep_map(&self.win_size, mine),
            pos3d: keep_map(&self.pos3d, mine),
            hidden_panel3d: keep_set(&self.hidden_panel3d, mine),
            scene_entities: self
                .scene_entities
                .iter()
                .filter(|e| mine(&e.lock().unwrap().node_id))
                .cloned()
                .collect(),
            view_mode: self.view_mode,
            file_nodes: keep_map(&self.file_nodes, mine),
            host_ports: keep_map(&self.host_ports, mine),
            notes: keep_map(&self.notes, mine),
            midi_ins: keep_map(&self.midi_ins, mine),
            midi_outs: keep_map(&self.midi_outs, mine),
            host_services: keep_map(&self.host_services, mine),
            boundary_ports: keep_map(&self.boundary_ports, mine),
            groups: keep_map(&self.groups, mine),
            net_nodes: keep_set(&self.net_nodes, mine),
            routers: keep_set(&self.routers, mine),
            node_labels: keep_map(&self.node_labels, mine),
            gateways: keep_set(&self.gateways, mine),
            uplinks: keep_map(&self.uplinks, mine),
            connections: keep_pairs(&self.connections, mine),
            mount_paths: keep_pair_map(&self.mount_paths, mine),
            fs_providers: keep_set(&self.fs_providers, mine),
            midi_links: keep_pairs(&self.midi_links, mine),
            net_links: keep_pairs(&self.net_links, mine),
            capture_links: keep_pairs(&self.capture_links, mine),
            clipboard_links: keep_pairs(&self.clipboard_links, mine),
            api_links: keep_pairs(&self.api_links, mine),
            serve_ports: keep_pair_map(&self.serve_ports, mine),
            capture_feeds: keep_map(&self.capture_feeds, mine),
            clipboard_boards: keep_map(&self.clipboard_boards, mine),
            api_nodes: keep_set(&self.api_nodes, mine),
            attached: keep_set(&self.attached, mine),
            serves: keep_map(&self.serves, mine),
            port_errors: keep_map(&self.port_errors, mine),
            node_args: keep_map(&self.node_args, mine),
            available: self.available.clone(),
            nodes: self.nodes.iter().filter(|n| mine(&n.id)).cloned().collect(),
            surfaces: self.surfaces.clone(),
            node_ws: self.node_ws.clone(),
            // The tab bar draws every tab, not just this one, so the names come
            // through a per-workspace narrowing untouched — like `workspaces`.
            workspaces: self.workspaces.clone(),
            workspace_names: self.workspace_names.clone(),
        }
    }

    /// Whether a given connection currently exists.
    pub fn wire_exists(&self, w: Wire) -> bool {
        match w {
            Wire::Bind(f, a) => self.connections.contains(&(f, a)),
            Wire::Midi(s, d) => self.midi_links.contains(&(s, d)),
            Wire::Serve(h, hp) => self.serves.get(&h) == Some(&hp),
            Wire::Capture(a, c) => self.capture_links.contains(&(a, c)),
            Wire::Clipboard(a, c) => self.clipboard_links.contains(&(a, c)),
            Wire::Api(a, n) => self.api_links.contains(&(a, n)),
            Wire::Net(app, net) => self.net_links.contains(&(app, net)),
        }
    }
}

/// Which saved-workspace relation a raw `(a, b)` pair belongs to. Used only to
/// round-trip wires touching an unplaced node (see `Server::unplaced_wires`),
/// where the node kinds needed to classify a live [`Wire`] aren't available.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum WireRel {
    Connection,
    Midi,
    Serve,
    NetLink,
    CaptureLink,
    ClipboardLink,
    ApiLink,
}

/// Every wire a saved workspace holds, tagged with the relation it belongs to.
/// The `.wk` file keeps one list per relation; a caller that treats wires
/// uniformly — deciding which of an instance's wires an edit added or took
/// away, spotting the ones that touch an unplaced node — wants them flat.
fn wires_of(saved: &Workspace) -> Vec<(WireRel, NodeId, NodeId)> {
    [
        (WireRel::Connection, &saved.connections),
        (WireRel::Midi, &saved.midi),
        (WireRel::Serve, &saved.serves),
        (WireRel::NetLink, &saved.net_links),
        (WireRel::CaptureLink, &saved.capture_links),
        (WireRel::ClipboardLink, &saved.clipboard_links),
        (WireRel::ApiLink, &saved.api_links),
    ]
    .into_iter()
    .flat_map(|(rel, pairs)| pairs.iter().map(move |&(a, b)| (rel, a, b)))
    .collect()
}

/// The two node ids a [`Wire`] joins.
fn wire_ends(w: Wire) -> (NodeId, NodeId) {
    match w {
        Wire::Bind(a, b)
        | Wire::Midi(a, b)
        | Wire::Serve(a, b)
        | Wire::Net(a, b)
        | Wire::Capture(a, b)
        | Wire::Clipboard(a, b)
        | Wire::Api(a, b) => (a, b),
    }
}

/// Drop entries of a `(NodeId, NodeId)`-keyed side map whose key is no longer a
/// live wire — called after a wire relation changes so per-wire overrides
/// (mount paths, container ports) don't outlive their connection.
fn prune_side_map<V>(map: &mut HashMap<(NodeId, NodeId), V>, live: &[(NodeId, NodeId)]) {
    let live: HashSet<(NodeId, NodeId)> = live.iter().copied().collect();
    map.retain(|pair, _| live.contains(pair));
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
    Text(NodeId, String),
    /// Re-toggle a connection between two nodes (connect is its own inverse).
    Wire(NodeId, NodeId),
    /// Undo a "one destination per source" wire (net membership, serve): drop
    /// the new `(src, new_dst)` and restore the link it displaced, if any.
    /// Plain [`Undo::Wire`] can't express this — toggling the new wire off just
    /// leaves `src` unwired instead of back on its previous destination.
    RewireUnique {
        src: NodeId,
        new_dst: NodeId,
        old_dst: Option<NodeId>,
    },
    /// Restore a node's previous capability token (`None` = the default).
    Token(NodeId, Option<Vec<u8>>),
    /// Put a `group`'s boundary wire back the way it was: `true` re-authors the
    /// `in`/`out` line, `false` takes it away again. One entry per edit, not
    /// per live wire the re-expansion moved — the line is what the user drew.
    Boundary(BoundaryWire, bool),
    /// Remove the nodes a create added. A list rather than one id because a
    /// create can produce a whole tree — a `group` node plus every node its
    /// instance expanded to — and [`Command::Undo`] pops exactly one entry, so
    /// an entry per node would take ten presses of Ctrl-Z to undo one create,
    /// leaving a half-dismantled live instance at each step.
    Uncreate(Vec<NodeId>),
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
    /// The tab's name, so undoing a close brings back what it was called.
    name: Option<String>,
    nodes: Vec<Snapshot>,
}

/// Everything needed to bring a removed node back exactly as it was.
struct Snapshot {
    ws: NodeId,
    /// The node itself, in the same shape the `.wk` file persists
    /// ([`crate::workspace::NodeSnap`]) — undo and load-time restore
    /// materialize through the same path.
    node: NodeSnap,
    /// A Volume's in-memory bytes: undo restores them; the `.wk` file
    /// deliberately does not persist content. Empty for every other kind.
    file_data: Vec<u8>,
    /// Every connection the node was part of, as raw node pairs.
    wires: Vec<(NodeId, NodeId)>,
    /// Every `group` line that named this node. A boundary wire is authored
    /// state on the *group*, not a wire between two nodes, so `wires` cannot
    /// hold it — and without it, undoing a delete would bring the node back
    /// with its instances no longer wired to it.
    boundary: Vec<BoundaryWire>,
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
    /// A router: bridges the Networks it is wired to. Unlike every other
    /// member of a network, it may be wired to several — that is what it is
    /// for — so its net links are plain pairs, not one-per-source.
    Router,
    /// An iroh uplink: extends the Network it's wired to onto a remote fabric.
    Iroh,
    /// A Veilid uplink: like Iroh, over Veilid's onion-routed network.
    Veilid,
    /// A yellow sticky note: a purely visual annotation, wired to nothing.
    Note,
    /// A Screen Capture node: grants wired apps captured frames.
    Capture,
    /// A Clipboard node: grants wired apps the HOST's system clipboard —
    /// `read` and `write` as separate, separately-attenuable token actions.
    Clipboard,
    /// The wk client API as a node: grants wired apps API access over their
    /// virtual network.
    Api,
    /// A hardware MIDI input node: the host opens a physical MIDI device and
    /// routes its messages to the app nodes it's wired to (a MIDI source).
    MidiIn,
    /// A host TCP service published into a Network as a named fabric peer —
    /// the reverse of a HostPort (fabric-dialable host service, not
    /// host-dialable fabric service).
    HostService,
    /// A workspace boundary port: the named, typed edge a connection crosses
    /// when this workspace is used from another one. Its direction, connection
    /// kind and name live in `Graph::boundary_ports`. In a plain tab it runs
    /// nothing and grants nothing — there is no other side yet.
    Boundary,
    /// A `group` node: one **instance** of another workspace. The node itself
    /// runs nothing — it is the handle for the nodes the expansion placed under
    /// derived ids (see [`crate::instancing`] and `Server::instances`).
    Group,
    /// A hardware MIDI output node: the host opens a physical MIDI destination
    /// and plays everything wired into it out of that port, so the canvas can
    /// drive an external synth. The mirror of [`Kind::MidiIn`].
    ///
    /// New variants go at the END of this enum: [`Server::save`] orders a
    /// workspace's nodes by `kind as u8`, so inserting one anywhere else would
    /// rewrite the node order of every existing `.wk` file on its next save.
    MidiOut,
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
    /// Which dependency each app node runs — its *type*. A node's name is its
    /// identity (what the fabric resolves and what `wk logs` takes), so the two
    /// are separate the way an image is separate from a container: several
    /// nodes can share one type, and none of them has to be called after it.
    pub node_deps: HashMap<NodeId, String>,
    /// What each app node is *called*. Assigned once, when the node is created
    /// or loaded, and authoritative from then on — a live guest mirrors it as
    /// its fabric name, but the name belongs to the node, not to the guest, so
    /// it survives a crash and exists while a component is still compiling.
    pub node_names: HashMap<NodeId, String>,
    /// Free 3D poses (`[x, y, z, yaw]`, world units) for nodes placed off the
    /// default layout cylinder. Side table: only posed nodes have entries.
    pub pos3d: HashMap<NodeId, [f32; 4]>,
    /// Nodes whose flat 2D panel is suppressed in the 3D world, leaving their
    /// `wk:scene` objects as their whole body. Side table: only hidden nodes
    /// have entries — the panel is the default, and the only way most nodes
    /// are visible at all.
    pub hidden_panel3d: HashSet<NodeId>,
    /// Canvas file nodes (in-memory or disk-backed) wired into apps.
    pub file_nodes: HashMap<NodeId, FileNode>,
    /// HostPort nodes (canvas id -> localhost port).
    pub host_ports: HashMap<NodeId, u16>,
    /// Note nodes' text (canvas id -> the sticky-note contents).
    pub note_text: HashMap<NodeId, String>,
    /// MidiIn nodes' target device name (canvas id -> device; empty = default),
    /// persisted so the node reconnects to the same hardware on reload.
    pub midi_ins: HashMap<NodeId, String>,
    /// MidiOut nodes' target device name (canvas id -> device; empty = default),
    /// persisted so the node reconnects to the same hardware on reload.
    pub midi_outs: HashMap<NodeId, String>,
    /// HostService nodes: the fabric name members dial and the host
    /// `addr:port` the connection bridges to. The fabric side listens on the
    /// target's port.
    pub host_services: HashMap<NodeId, HostService>,
    /// Boundary-port nodes: what each one is called, which direction it faces
    /// and what kind of connection may cross it. Side table keyed by node id,
    /// like every other kind's payload.
    pub boundary_ports: HashMap<NodeId, BoundaryPort>,
    /// `group` nodes: which definition each instantiates, and how this canvas
    /// wires that definition's boundary ports. This is the *authored* fact —
    /// what [`Server::save`] writes back. What it expands to is runtime state
    /// (`Server::instances`), rebuilt from this on every load.
    pub groups: HashMap<NodeId, GroupNode>,

    /// Volume binds as (volume id, app node id).
    pub connections: Vec<(NodeId, NodeId)>,
    /// Where a bind mounts inside its app, keyed by (volume, app). Absent = the
    /// default (the volume's name at the filesystem root).
    pub mount_paths: HashMap<(NodeId, NodeId), String>,
    /// MIDI connections as (source node id, destination node id).
    pub midi_links: Vec<(NodeId, NodeId)>,
    /// Serve wiring: (served node id, HostPort id).
    pub serve_links: Vec<(NodeId, NodeId)>,
    /// The guest (container) port a serve wire forwards to, keyed by
    /// (served, hostport). Absent = the HostPort's own port (forward verbatim).
    /// Only meaningful for the wasi:sockets forward path; http serve ignores it.
    pub serve_ports: HashMap<(NodeId, NodeId), u16>,
    /// Network membership wires, as (app node id, Network node id).
    pub net_links: Vec<(NodeId, NodeId)>,
    /// Screen-capture grants, as (app node id, Capture node id).
    pub capture_links: Vec<(NodeId, NodeId)>,
    /// Host-clipboard grants, as (app node id, Clipboard node id).
    pub clipboard_links: Vec<(NodeId, NodeId)>,
    /// API grants, as (app node id, Api node id).
    pub api_links: Vec<(NodeId, NodeId)>,

    /// App nodes' *custom* capability tokens (serialized Biscuits), set via
    /// `wk token`. Absent = the workspace's default node token ("use what
    /// you're wired to"). Side table keyed by node id; persisted hex in the
    /// `.wk` file.
    pub node_tokens: HashMap<NodeId, Vec<u8>>,
    /// Iroh uplink nodes' ed25519 secrets, so a node's ticket (its dialable
    /// identity) survives restarts. The peer ticket it dials lives in
    /// `node_args`. Side table keyed by node id.
    pub iroh_secrets: HashMap<NodeId, [u8; 32]>,
    /// Veilid uplink nodes' DHT owner keypairs (string form), the Veilid
    /// equivalent of `iroh_secrets`. Side table keyed by node id.
    pub veilid_ids: HashMap<NodeId, String>,

    /// The workspaces (tabs) in this document, in order — including empty ones.
    pub workspaces: Vec<NodeId>,
    /// Workspace names (`name "voice"`), keyed by workspace id. A side table so
    /// `workspaces` stays a plain ordered id list; only named tabs have an
    /// entry, and an entry may be blank (a workspace named the empty string).
    /// The graph must carry this or [`Server::save`] — a full re-projection —
    /// would write every name out of existence on the first clean exit.
    pub workspace_names: HashMap<NodeId, String>,
    /// The workspace's launchable dependencies.
    pub available: Vec<Dependency>,
}

/// A HostService node's configuration: a host TCP service published into a
/// Network as a named fabric peer. `target` is the host `addr:port` bridged
/// to; the fabric listener uses the target's port, so `name:port` inside the
/// net mirrors the host service one-to-one.
#[derive(Clone, Debug, PartialEq)]
pub struct HostService {
    /// The fabric name members of the Network dial.
    pub name: String,
    /// The host `addr:port` each accepted connection is bridged to.
    pub target: String,
}

/// A boundary port's declaration: the name a wire from outside picks it by,
/// which edge of the workspace it sits on, and what may cross it.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundaryPort {
    pub name: String,
    pub dir: PortDir,
    pub kind: PortKind,
}

/// What a client needs to draw a `group` node: which definition it is an
/// instance of, the boundary ports that definition declares (the instance's own
/// edges, in file order), and how many nodes it is currently running.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupInfo {
    pub definition: String,
    pub ports: Vec<BoundaryPort>,
    /// The boundary wires this canvas has authored, by port name — what the
    /// `in`/`out` lines of the group's block say. A client draws these itself:
    /// the live wire they become lands on a *derived* node, which is in the
    /// instance rather than on this canvas, so nothing else on the tab would
    /// show that the instance is connected at all.
    pub in_wires: Vec<(String, NodeId)>,
    pub out_wires: Vec<(String, NodeId)>,
    /// Live nodes in this instance and everything nested inside it. Zero means
    /// the definition is empty — or that the expansion could not resolve.
    pub nodes: usize,
}

/// A `group` node's declaration: the definition it instantiates and the wires
/// crossing that definition's boundary, by port name. The endpoints are nodes
/// on *this* canvas; what they end up joined to is decided by the expansion,
/// which collapses each port away (see [`crate::instancing`]).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupNode {
    pub definition: String,
    /// What this instance is called, when the file says (see
    /// `SnapKind::Group::name`). Absent leaves the naming to the expander,
    /// which falls back to the definition's name.
    pub name: Option<String>,
    /// `in "<port>" "<node>"`: this canvas feeding one of the definition's
    /// in-ports.
    pub in_wires: Vec<(String, NodeId)>,
    /// `out "<port>" "<node>"`: one of the definition's out-ports reaching a
    /// node on this canvas.
    pub out_wires: Vec<(String, NodeId)>,
}

/// One live instance: what a `group` node expanded to. Pure runtime state,
/// rebuilt from the group node on every load — an instance is deliberately not
/// a workspace, so none of it is ever written back to the `.wk` file.
struct InstanceRec {
    /// The tab the whole tree is shown in. Instance ids stay out of
    /// `Graph::workspaces` (they are not tabs), so this is what a workspace
    /// teardown and `wk ps` go by.
    tab: NodeId,
    /// The instance this one sits inside, or `None` for a `group` written
    /// directly in a tab. Deleting an instance takes its descendants with it,
    /// and an instance's `wk ps` label is built from its parent's.
    parent: Option<NodeId>,
    /// The definition this instantiates — the instance's *type*.
    definition: String,
    /// What this instance is called (scoped by any instance it sits inside).
    /// Every node it placed is named after it, so this is the one string that
    /// tells two instances of a definition apart.
    name: String,
    /// Every node this instance placed, so a teardown can find them all even
    /// after their wiring is gone.
    nodes: Vec<NodeId>,
    /// Every wire the expansion actually made. The endpoints do not say who
    /// made a wire: a definition that passes a connection straight through, in
    /// one port and out the complementary one, collapses to a wire between two
    /// nodes of the *parent* canvas. That one has to be kept out of the `.wk`
    /// file and torn down with the group like any other, and only this record
    /// can find it.
    wires: Vec<(WireRel, NodeId, NodeId)>,
}

impl HostService {
    /// The TCP port of `target` — also the fabric listen port. `None` when
    /// the target does not parse, which the reconciler treats as "not ready"
    /// rather than an error.
    pub fn port(&self) -> Option<u16> {
        self.target.rsplit_once(':')?.1.parse().ok()
    }
}

/// Serves one accepted fabric API connection: `(stream, token)` — the
/// connection's socketpair end and the wired node's capability token. The
/// runtime installs this (it closes over `wk-api`'s `serve_client`, which
/// depends on this crate — a dependency inversion), and `sync_apis` calls it
/// per connection.
pub type ApiConnServer = Arc<dyn Fn(std::os::unix::net::UnixStream, Vec<u8>) + Send + Sync>;

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
    /// Active file mounts: (file, app) -> (mount name, the app's fs, writable).
    /// Stores the name+fs so a mount can be torn down even after either node is
    /// gone, and the mode so a write-permission change remounts. Mirrors
    /// `graph.connections`; reconciled by `sync_mounts`.
    mounted: HashMap<(NodeId, NodeId), (String, crate::vfs::SharedFs, bool)>,
    /// Active MIDI routes: (src, dst) currently in the router. Mirrors
    /// `graph.midi_links`; reconciled by `sync_midi`.
    routed: HashSet<(NodeId, NodeId)>,
    /// Running fabric API endpoints: app node id -> (Api node id, kill switch,
    /// fingerprint of the token the listener was started with). A subset of
    /// `graph.api_links` — an entry exists once the app's stack is up and its
    /// token allows the wire; a token swap restarts the endpoint so new
    /// connections carry the new token. Reconciled by `sync_apis`.
    api_serves: HashMap<NodeId, (NodeId, Arc<AtomicBool>, u64)>,
    /// Running HostService fabric listeners: service node id -> (kill switch,
    /// fingerprint of the (net, name, target) it was started with). A subset
    /// of `graph.net_links`; any config or wiring change restarts the
    /// listener. Reconciled by `sync_host_services`.
    host_service_serves: HashMap<NodeId, (Arc<AtomicBool>, u64)>,
    /// The installed API connection server (see [`ApiConnServer`]); `None`
    /// until the runtime injects it (headless embedding without wk-api simply
    /// starts no endpoints).
    api_conn_server: Option<ApiConnServer>,
    /// Currently *running* servers: served node id -> (HostPort id, kill switch).
    /// A subset of `graph.serve_links` — an entry appears only once the node is
    /// ready (a wasi:http node dispatched per request, or a fabric node with a
    /// TCP+UDP forward into its network) and the port bound. Reconciled by
    /// `sync_serves`.
    pub serves: HashMap<NodeId, (NodeId, Arc<AtomicBool>)>,
    /// Nodes whose run was requested while their component was still compiling.
    /// The tick loop starts each one as soon as it's ready — so clicking play on
    /// a big container that's mid-compile is never lost to a race.
    pending_run: HashSet<NodeId>,
    /// HostPorts whose last bind attempt failed — keyed by HostPort node id, the
    /// value a human message (e.g. the localhost port is already in use by
    /// another process). Surfaced in the snapshot so `wk ps`/the UI can warn;
    /// cleared once the port binds or the serve wire is removed.
    port_errors: HashMap<NodeId, String>,
    /// The fabric-side bridge behind each live Router node. Present only while
    /// the router is wired to two or more Networks (see `sync_routers`).
    routers: HashMap<NodeId, Arc<wk_fabric::netstack::RouterPort>>,
    /// Running uplinks (Iroh or Veilid), one per uplink node. Dropping one
    /// closes its endpoint and detaches its trunk.
    uplinks: HashMap<NodeId, UplinkHandle>,
    /// Each Capture node's frame slot (the client fills it; wired apps read
    /// it through their `capture_src`, kept in sync by [`Self::sync_captures`]).
    capture_feeds: HashMap<NodeId, crate::capture::SharedFrameSlot>,
    /// Each Clipboard node's board (the client pumps the real host clipboard
    /// through it; wired apps read and write it through their `clip_src`,
    /// kept in sync by [`Self::sync_clipboard`]). wk-server never touches a
    /// platform clipboard API itself — a Clipboard node with no client
    /// pumping it is simply an empty one.
    clipboard_boards: HashMap<NodeId, crate::clipboard::SharedBoard>,
    /// Open hardware MIDI inputs, one per MidiIn node: holds the device
    /// connection alive (dropping it closes the device); its callback feeds the
    /// MIDI router as the node. Pure runtime state, rebuilt from `graph.midi_ins`
    /// on load.
    midi_devices: HashMap<NodeId, crate::midihw::MidiDevice>,
    /// Open hardware MIDI outputs, one per MidiOut node. Each holds the device
    /// connection alive (dropping it closes the device and stops its pump) and
    /// owns the inbox the router delivers into. Pure runtime state, rebuilt
    /// from `graph.midi_outs` on load.
    midi_out_devices: HashMap<NodeId, crate::midihw::MidiOutDevice>,
    /// Nodes a CLI client has `attach`ed to (owning their terminal I/O). The
    /// windowed UI treats these as detached: it stops draining/feeding their
    /// terminal so the two don't fight over the stream. Pure runtime state.
    attached: std::collections::HashSet<NodeId>,

    /// Nodes present in the loaded `.wk` file that couldn't be materialized —
    /// an app whose dependency isn't in the list (renamed/removed), or an
    /// uplink whose endpoint failed to start (offline). Kept as `(ws, snap)`
    /// so [`Self::save`] round-trips them verbatim instead of silently
    /// deleting them (which, for an uplink, would lose its identity secret and
    /// orphan every peer holding its ticket). Populated only at load.
    unplaced: Vec<(NodeId, NodeSnap)>,
    /// Saved wires that touch an unplaced node (so they never entered the live
    /// graph), kept as `(ws, relation, a, b)` so save round-trips them and an
    /// unplaced node comes back still wired once it can materialize.
    unplaced_wires: Vec<(NodeId, WireRel, NodeId, NodeId)>,
    /// Workspaces the runtime deliberately does not run: `tab #false`
    /// *definitions*, which exist to be used from elsewhere rather than opened.
    /// Kept as `(position in the loaded document, the workspace)` so
    /// [`Self::save`] — a full re-projection from the live graph, which knows
    /// nothing about them — can splice them back exactly where they were
    /// instead of writing them out of existence. The `unplaced` precedent, one
    /// level up: there a node the runtime can't model, here a whole workspace
    /// it won't.
    authored: Vec<(usize, Workspace)>,
    /// Live instances, keyed by instance id — a `group` node's own id at the
    /// top level, a derived id when the group is written inside a definition.
    /// A `BTreeMap` because the order decides an instance's label when a tab
    /// holds two of the same definition, and that must not wander between runs.
    instances: BTreeMap<NodeId, InstanceRec>,
    /// App↔app wires whose kind (MIDI vs provider mount) can't be decided
    /// yet: `serves_fs()` is unknowable while an endpoint's component is
    /// still compiling. Re-tried each tick; applied through the normal
    /// classification once both endpoints have published their setup.
    pending_app_wires: Vec<(NodeId, NodeId)>,

    /// Node-capability auth: the token service's public key plus the base node
    /// token every app node holds by default (its authority block carries the
    /// "use what you're wired to" rule). `None` = enforcement off (a server
    /// embedded without a token service, and most tests) — wiring effects then
    /// apply unconditionally, as before node tokens existed.
    node_auth: Option<(biscuit_auth::PublicKey, Vec<u8>)>,
    /// Memoized node-use decisions for the per-tick reconcilers: the cached
    /// verdict is valid while its fingerprint (token bytes + the node's wire
    /// set) is unchanged. Keyed by (node, kind, target, action).
    auth_cache: HashMap<(NodeId, &'static str, NodeId, &'static str), (u64, bool)>,

    /// Inverse-command history for [`Command::Undo`].
    undo: Vec<Undo>,

    next_port: u16,
    file_seq: u32,
    host_seq: u32,

    /// The latest `wk view` request (sequence, mode); the sequence starts at 0
    /// (nothing asked yet) and advances on each request.
    view_mode: (u64, ViewMode),

    /// `import` directives from the loaded top-level `.wk` file, re-emitted on
    /// save so the composition is preserved.
    imports: Vec<String>,
    /// Dependency names / workspace ids that came from an import — omitted on
    /// save (they live in the imported files). See [`Document::load_resolved`].
    imported_deps: HashSet<String>,
    imported_workspaces: HashSet<NodeId>,
    /// The tab the loader invented for a file that declared none (see
    /// [`Document::scratch_tab`]). It is not authored content: `save` leaves it
    /// out of the file for as long as it stays empty, so opening a file of pure
    /// definitions cannot write a stray `workspace { }` block into it.
    scratch_tab: Option<NodeId>,
}

impl Server {
    /// Create a server and instantiate every workspace in the document (all tabs
    /// run at once). `path` is the `.wk` file to save back to.
    pub fn new(doc: &Document, path: PathBuf) -> Result<Self, String> {
        // Check the document's instancing before anything starts. A `group`
        // naming no definition — or one that contains itself — has no
        // expansion, and running half of what the file says is worse than not
        // running: the missing half would be saved back as gone. This lives
        // here rather than in the loader because `wk add`/`wk remove` (and
        // `wk pull`, which falls back to an empty document on any load error)
        // must keep working on a file whose instancing is broken.
        crate::instancing::expand(doc)?;
        let host = PluginHost::new().map_err(|e| format!("{e:#}"))?;
        let mut server = Server {
            host,
            registry: Arc::new(Mutex::new(Vec::new())),
            node_reg: Arc::new(Mutex::new(Vec::new())),
            workspace_path: path,
            graph: Graph {
                available: doc.dependencies.clone(),
                // Only tabs enter the live graph: a `tab #false` definition is
                // not run, not shown, and not projected back out by `save` —
                // it is carried verbatim in `authored` instead.
                workspaces: doc
                    .workspaces
                    .iter()
                    .filter(|w| w.tab)
                    .map(|w| w.id)
                    .collect(),
                workspace_names: doc
                    .workspaces
                    .iter()
                    .filter(|w| w.tab)
                    .filter_map(|w| w.name.clone().map(|n| (w.id, n)))
                    .collect(),
                ..Graph::default()
            },
            mounted: HashMap::new(),
            routed: HashSet::new(),
            serves: HashMap::new(),
            api_serves: HashMap::new(),
            host_service_serves: HashMap::new(),
            api_conn_server: None,
            pending_run: HashSet::new(),
            port_errors: HashMap::new(),
            routers: HashMap::new(),
            uplinks: HashMap::new(),
            capture_feeds: HashMap::new(),
            clipboard_boards: HashMap::new(),
            midi_devices: HashMap::new(),
            midi_out_devices: HashMap::new(),
            attached: std::collections::HashSet::new(),
            unplaced: Vec::new(),
            unplaced_wires: Vec::new(),
            authored: doc
                .workspaces
                .iter()
                .enumerate()
                .filter(|(_, w)| !w.tab)
                .map(|(i, w)| (i, w.clone()))
                .collect(),
            instances: BTreeMap::new(),
            pending_app_wires: Vec::new(),
            node_auth: None,
            auth_cache: HashMap::new(),
            undo: Vec::new(),
            next_port: 8080,
            file_seq: 0,
            host_seq: 0,
            view_mode: (0, ViewMode::Flat),
            imports: doc.imports.clone(),
            imported_deps: doc.imported_deps.clone(),
            imported_workspaces: doc.imported_workspaces.clone(),
            scratch_tab: doc.scratch_tab,
        };
        for ws in doc.workspaces.iter().filter(|w| w.tab) {
            server.instantiate(ws);
        }
        Ok(server)
    }

    /// Whether this node was placed by an expansion rather than written in the
    /// file. Derived nodes live in an *instance*, never in a tab, so the check
    /// is simply "is its workspace an instance" — which is also why nothing
    /// that iterates `graph.workspaces` can reach one by accident.
    fn is_derived(&self, id: NodeId) -> bool {
        self.graph
            .nodes
            .get(&id)
            .is_some_and(|rec| self.instances.contains_key(&rec.ws))
    }

    /// Spawn one workspace's nodes and re-apply its wiring (used at load).
    /// Every node materializes through [`Self::materialize`] — the same path
    /// undo uses — then the saved wires are re-established generically. A wire
    /// whose node is still compiling is recorded as desired state and applied
    /// by the tick loop's reconcilers once the node is ready.
    fn instantiate(&mut self, saved: &Workspace) {
        for snap in &saved.nodes {
            self.materialize(saved.id, snap, &[]);
            // A node that failed to materialize (unknown dependency, offline
            // uplink) is preserved verbatim so save doesn't delete it.
            if !self.node_exists(snap.id) {
                self.unplaced.push((saved.id, snap.clone()));
            }
        }
        // Preserve wires touching an unplaced node (rewire skips them since the
        // endpoint doesn't exist) so they come back when it materializes.
        let orphan: HashSet<NodeId> = self
            .unplaced
            .iter()
            .filter(|(w, _)| *w == saved.id)
            .map(|(_, s)| s.id)
            .collect();
        for (rel, a, b) in wires_of(saved) {
            if orphan.contains(&a) || orphan.contains(&b) {
                self.unplaced_wires.push((saved.id, rel, a, b));
            }
        }
        self.apply_wires(saved);
        // Finally the instances. A `group`'s boundary wires reach nodes on
        // this canvas, so every one of them has to exist before the expansion
        // lands — which is why this is a pass of its own rather than part of
        // materializing the group node.
        for snap in &saved.nodes {
            if matches!(snap.kind, SnapKind::Group { .. }) {
                self.expand_group(saved.id, snap.id);
            }
        }
    }

    /// Materialize one instance's expanded content: its nodes (under derived
    /// ids, in the instance's own "workspace") and its wiring. Unlike a tab, a
    /// node that fails to materialize is *not* remembered — it is derived, so
    /// the file already holds everything needed to try again next run, and
    /// recording it would leave a wire pointing into a workspace that is not a
    /// tab. Its wires are simply never applied.
    ///
    /// Returns the wires it made, which is what the instance is remembered by:
    /// see [`InstanceRec::wires`].
    fn instantiate_content(&mut self, content: &Workspace) -> Vec<(WireRel, NodeId, NodeId)> {
        for snap in &content.nodes {
            self.materialize(content.id, snap, &[]);
        }
        self.apply_wires(content)
    }

    /// Re-establish one saved workspace's wiring, each relation as its own kind.
    /// Returns the wires this call actually made — a wire already there is not
    /// among them, since it belongs to whoever made it first.
    fn apply_wires(&mut self, saved: &Workspace) -> Vec<(WireRel, NodeId, NodeId)> {
        let mut made: Vec<(WireRel, NodeId, NodeId)> = Vec::new();
        // Re-apply each saved relation AS ITS OWN KIND — the file already
        // distinguishes them. Flattening everything through `rewire` (which
        // re-classifies by node kind) mistyped provider mounts: an app→app
        // `connection` classifies by `serves_fs()`, which is still false
        // while the provider's component compiles in the background, so the
        // wire landed in the MIDI relation and the mount never happened.
        let exists =
            |s: &Self, a: NodeId, b: NodeId| s.node_exists(a) && s.node_exists(b) && !s.wired(a, b);
        for &(f, a) in &saved.connections {
            if exists(self, f, a) {
                self.toggle_file(f, a);
                made.push((WireRel::Connection, f, a));
            }
        }
        for &(s, d) in &saved.midi {
            if exists(self, s, d) {
                self.toggle_midi(s, d);
                made.push((WireRel::Midi, s, d));
            }
        }
        for &(h, p) in &saved.serves {
            if exists(self, h, p) {
                self.toggle_serve(h, p);
                made.push((WireRel::Serve, h, p));
            }
        }
        for &(m, n) in &saved.net_links {
            if exists(self, m, n) {
                self.toggle_net(m, n);
                made.push((WireRel::NetLink, m, n));
            }
        }
        for &(a, c) in &saved.capture_links {
            if exists(self, a, c) {
                self.toggle_capture(a, c);
                made.push((WireRel::CaptureLink, a, c));
            }
        }
        for &(a, c) in &saved.clipboard_links {
            if exists(self, a, c) {
                self.toggle_clipboard(a, c);
                made.push((WireRel::ClipboardLink, a, c));
            }
        }
        for &(a, n) in &saved.api_links {
            if exists(self, a, n) {
                self.toggle_api(a, n);
                made.push((WireRel::ApiLink, a, n));
            }
        }
        // Restore per-bind mount paths, then re-apply so mounts land at them.
        for (&pair, path) in &saved.mount_paths {
            self.graph.mount_paths.insert(pair, path.clone());
        }
        self.sync_mounts();
        // Restore per-serve container ports, then re-bind so they take effect.
        for (&pair, &port) in &saved.serve_ports {
            self.graph.serve_ports.insert(pair, port);
        }
        self.sync_serves();
        made
    }

    /// Resolve what one `group` node stands for, without materializing any of
    /// it: the definition's content under derived ids, its nested instances,
    /// and its boundary wires already collapsed onto this canvas's nodes.
    ///
    /// The document handed to the expander is the authored definitions plus
    /// *this* canvas — every node of it except the other groups, which each
    /// resolve on their own and would otherwise come back as a second copy of
    /// an instance that is already live. The canvas has to be there because a
    /// boundary wire is typechecked against the node on its far end.
    fn resolve_group(
        &self,
        ws: NodeId,
        id: NodeId,
    ) -> Result<Vec<crate::instancing::Instance>, String> {
        let mut nodes: Vec<NodeSnap> = self
            .graph
            .nodes
            .iter()
            .filter(|(&n, rec)| rec.ws == ws && (n == id || rec.kind != Kind::Group))
            .filter_map(|(&n, _)| self.node_snap(n))
            .collect();
        // A node the file holds but the runtime could not place is still a
        // legal endpoint: the wire to it is preserved for when it can
        // materialize, and refusing the expansion here would take the whole
        // instance down with one unresolved dependency.
        nodes.extend(
            self.unplaced
                .iter()
                .filter(|(w, _)| *w == ws)
                .map(|(_, s)| s.clone()),
        );
        // `graph.nodes` is a hash map, so sort: nothing about an expansion
        // should depend on the order two unrelated neighbours came out in.
        nodes.sort_by_key(|n| n.id);
        let mut workspaces: Vec<Workspace> = self.authored.iter().map(|(_, w)| w.clone()).collect();
        workspaces.push(Workspace {
            id: ws,
            nodes,
            ..Workspace::new()
        });
        crate::instancing::expand(&Document {
            workspaces,
            ..Document::empty()
        })
    }

    /// Bring one `group` node's instance to life: the definition's nodes under
    /// derived ids, in the instance's own workspace, plus the wiring — the
    /// definition's own, and the boundary wires already collapsed onto this
    /// canvas's nodes. Nested groups come with it, each its own instance.
    ///
    /// The expansion is re-resolved from the definitions the file carries
    /// rather than from a list computed at startup, so the one path serves
    /// load, undo and reopening a closed tab alike.
    fn expand_group(&mut self, ws: NodeId, id: NodeId) {
        if self.instances.contains_key(&id) {
            return; // already live
        }
        let instances = match self.resolve_group(ws, id) {
            Ok(instances) => instances,
            // `Server::new` already refused a document whose instancing does
            // not resolve, so this only fires for a group edited at runtime.
            Err(e) => {
                eprintln!("wk: cannot expand group: {e}");
                return;
            }
        };
        // Parents come first, so a nested instance's boundary endpoints are
        // already placed by the time it is instantiated.
        for inst in instances {
            self.instances.insert(
                inst.id,
                InstanceRec {
                    tab: inst.tab,
                    parent: inst.parent,
                    definition: inst.definition.clone(),
                    name: inst.name.clone(),
                    nodes: inst.content.nodes.iter().map(|n| n.id).collect(),
                    wires: Vec::new(),
                },
            );
            // The record goes in before its content, so anything the
            // materialization reaches already sees these nodes as an
            // instance's, and is completed with the wiring that turned out to
            // be made — which is not every wire the expansion asked for, since
            // one already on the canvas belongs to whoever made it first.
            let made = self.instantiate_content(&inst.content);
            if let Some(rec) = self.instances.get_mut(&inst.id) {
                rec.wires = made;
            }
        }
    }

    /// Whether this exact `in`/`out` line is already written in its group.
    fn boundary_wired(&self, bw: &BoundaryWire) -> bool {
        self.graph.groups.get(&bw.group).is_some_and(|g| {
            let list = match bw.dir {
                PortDir::In => &g.in_wires,
                PortDir::Out => &g.out_wires,
            };
            list.iter().any(|(p, n)| *p == bw.port && *n == bw.node)
        })
    }

    /// Author (or unauthor) one line of a `group`'s block — a boundary wire —
    /// and bring the instance's live wiring in line with it.
    ///
    /// The edit is to the *group node*, not to the graph: `in "notes" "<id>"`
    /// is what the `.wk` file holds and what [`Self::save`] writes back. What
    /// it produces is decided by re-resolving the instance, so a wire the
    /// author adds through the canvas and one they type into the file take
    /// exactly the same path.
    ///
    /// A line the expansion refuses (a port that isn't there, a neighbour that
    /// can't form that kind of connection) is rolled back and reported rather
    /// than left in a group that no longer expands.
    fn set_boundary_wire(&mut self, bw: &BoundaryWire, wired: bool) {
        let Some(ws) = self.graph.nodes.get(&bw.group).map(|rec| rec.ws) else {
            return;
        };
        let Some(before) = self.graph.groups.get(&bw.group).cloned() else {
            eprintln!("wk: {} is not an instance", bw.group);
            return;
        };
        let g = self.graph.groups.get_mut(&bw.group).expect("just cloned");
        let list = match bw.dir {
            PortDir::In => &mut g.in_wires,
            PortDir::Out => &mut g.out_wires,
        };
        let at = list
            .iter()
            .position(|(p, n)| *p == bw.port && *n == bw.node);
        match (wired, at) {
            (true, None) => list.push((bw.port.clone(), bw.node)),
            (false, Some(i)) => {
                list.remove(i);
            }
            // Already as asked: no edit, and no re-expansion to churn the
            // instance's wiring for nothing.
            _ => return,
        }
        if let Err(e) = self.rewire_group(ws, bw.group) {
            eprintln!("wk: {e}");
            // Nothing was applied — the expansion failed before it touched the
            // graph — so putting the authored lines back is the whole undo.
            self.graph.groups.insert(bw.group, before);
        }
    }

    /// Re-resolve a live instance and move its wiring to match, leaving its
    /// nodes (and their guests) alone.
    ///
    /// A boundary wire decides which wires an instance has, never which nodes:
    /// the derived ids come from the definition and the group's id, so they are
    /// the same before and after. Tearing the instance down and expanding it
    /// again would give the same answer — and restart every guest inside it to
    /// get there.
    fn rewire_group(&mut self, ws: NodeId, id: NodeId) -> Result<(), String> {
        for inst in self.resolve_group(ws, id)? {
            let want = wires_of(&inst.content);
            // Not live (its expansion failed at load): nothing to move.
            let Some(rec) = self.instances.get_mut(&inst.id) else {
                continue;
            };
            let had = std::mem::take(&mut rec.wires);
            for &(rel, a, b) in had.iter().filter(|w| !want.contains(w)) {
                self.unwire_rel(rel, a, b);
            }
            let mut kept: Vec<(WireRel, NodeId, NodeId)> =
                had.into_iter().filter(|w| want.contains(w)).collect();
            kept.extend(self.apply_wires(&inst.content));
            if let Some(rec) = self.instances.get_mut(&inst.id) {
                rec.wires = kept;
            }
        }
        Ok(())
    }

    /// Remove a `group` node: tear down the instance it stands for (and every
    /// instance nested inside it) before forgetting the node itself. Anything
    /// less would leave live guests running with no handle to reach them.
    fn remove_group(&mut self, id: NodeId) {
        self.tear_down_instance(id);
        self.graph.groups.remove(&id);
        self.forget(id);
    }

    /// Stop and forget one instance and everything nested inside it.
    fn tear_down_instance(&mut self, id: NodeId) {
        let nested: Vec<NodeId> = self
            .instances
            .iter()
            .filter(|(_, rec)| rec.parent == Some(id))
            .map(|(&i, _)| i)
            .collect();
        for child in nested {
            self.tear_down_instance(child);
        }
        let Some(rec) = self.instances.remove(&id) else {
            return;
        };
        // Wires first: removing the nodes takes their wiring with it, but a
        // pass-through wire has no node of this instance on either end and
        // would otherwise be left behind on the parent's canvas.
        for (rel, a, b) in rec.wires {
            self.unwire_rel(rel, a, b);
        }
        for node in rec.nodes {
            self.remove_any(node);
        }
    }

    /// Undo one wire an expansion made, if it is still there. Each relation
    /// toggles through its own path so the runtime effect (a mount, a route, a
    /// grant) is undone with it.
    fn unwire_rel(&mut self, rel: WireRel, a: NodeId, b: NodeId) {
        let links = match rel {
            WireRel::Connection => &self.graph.connections,
            WireRel::Midi => &self.graph.midi_links,
            WireRel::Serve => &self.graph.serve_links,
            WireRel::NetLink => &self.graph.net_links,
            WireRel::CaptureLink => &self.graph.capture_links,
            WireRel::ClipboardLink => &self.graph.clipboard_links,
            WireRel::ApiLink => &self.graph.api_links,
        };
        if !links.contains(&(a, b)) {
            return; // already gone with one of its endpoints
        }
        match rel {
            WireRel::Connection => self.toggle_file(a, b),
            WireRel::Midi => self.toggle_midi(a, b),
            WireRel::Serve => self.toggle_serve(a, b),
            WireRel::NetLink => self.toggle_net(a, b),
            WireRel::CaptureLink => self.toggle_capture(a, b),
            WireRel::ClipboardLink => self.toggle_clipboard(a, b),
            WireRel::ApiLink => self.toggle_api(a, b),
        }
    }

    /// The name a node is known by when the file does not say: two words
    /// derived from its id (see [`crate::nodename`]), walked on the rare
    /// collision.
    ///
    /// Derived, not chosen: a node's name is its address on the fabric and the
    /// handle every CLI verb takes, so it must be unique, stable for the life
    /// of the node, and free of any claim about what the node *is* — a type
    /// can be shared, and a name that reads like one invites exactly the
    /// confusion this replaced.
    ///
    /// Checked against the graph rather than the live registry: a node whose
    /// component is still compiling has no guest yet, and two created in quick
    /// succession would otherwise be handed the same name.
    fn generated_node_name(&self, id: NodeId) -> String {
        let taken = |n: &str| self.graph.node_names.values().any(|taken| taken == n);
        (0..)
            .map(|nth| crate::nodename::generated(id, nth))
            .find(|candidate| !taken(candidate))
            .expect("an unbounded walk finds a free name")
    }

    /// How `wk ps` names an instance: the definition's name, prefixed by the
    /// instance it sits in, and numbered when a canvas holds more than one
    /// instance of the same definition — otherwise two copies of `voice` would
    /// print identically and neither could be told from the other.
    fn instance_label(&self, id: NodeId) -> String {
        self.instances
            .get(&id)
            .map(|rec| rec.name.clone())
            .unwrap_or_default()
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

    /// Whether `id` is a Gateway node (a Network that grants host access).
    fn is_gateway(&self, id: NodeId) -> bool {
        self.kind_of(id) == Some(Kind::Gateway)
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
        // The name comes from the id, so it is settled the moment the id is,
        // and nothing a later create does can move a hostname out from under a
        // wire that already dials it.
        let name = self.generated_node_name(id);
        if let Err(e) = self.host.spawn(
            &dep.local_path(),
            &name,
            id,
            &dep.effective_args(),
            self.registry.clone(),
            self.node_reg.clone(),
            Vec::new(),
            dep.container(),
        ) {
            eprintln!("failed to launch {}: {e:#}", dep.name);
            return;
        }
        self.place(id, Kind::App, ws, pos, [360.0, 260.0]);
        self.graph.node_deps.insert(id, dep.name.clone());
        self.graph.node_names.insert(id, name);
        self.graph.node_args.insert(id, dep.args.clone());
        self.write_token_file(id);
    }

    /// Create a new, empty in-memory Volume node at `pos` in workspace `ws`.
    fn add_virtual_file(&mut self, pos: [f32; 2], ws: NodeId) {
        self.file_seq += 1;
        let id = self.alloc_id();
        self.place(id, Kind::File, ws, pos, [FILE_W, FILE_H]);
        self.graph.file_nodes.insert(
            id,
            FileNode::Volume(Volume {
                name: format!("file{}", self.file_seq),
                data: Arc::new(Mutex::new(Vec::new())),
                persist: false,
            }),
        );
    }

    /// Create a BindMount node backed by a fresh host file (`host<n>`).
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
            .insert(id, FileNode::Bind(BindMount { name, path }));
    }

    /// Create a BindMount node already pointed at `path` — what an OS
    /// drag-and-drop onto the canvas delivers. Same node the palette's
    /// create-then-point produces, minus the placeholder file: the dropped
    /// path exists by definition.
    fn add_host_mount(&mut self, path: PathBuf, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        let name = host_file_name(&path);
        self.place(id, Kind::File, ws, pos, [FILE_W, FILE_H]);
        self.graph
            .file_nodes
            .insert(id, FileNode::Bind(BindMount { name, path }));
        self.sync_mounts();
    }

    /// Create a HostPort node at `pos` (auto-assigned localhost port).
    fn add_host_port(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        let port = self.next_port;
        self.next_port = self.next_port.wrapping_add(1).max(8080);
        self.place(id, Kind::Port, ws, pos, [FILE_W, FILE_H]);
        self.graph.host_ports.insert(id, port);
    }

    /// Add a yellow note (annotation) node, seeded with placeholder text.
    fn add_note(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.place(id, Kind::Note, ws, pos, [NOTE_W, NOTE_H]);
        self.graph.note_text.insert(id, "note".to_string());
    }

    /// Add a Screen Capture capability node (its frame slot starts empty; the
    /// client fills it while an app is wired).
    fn add_capture_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.place(id, Kind::Capture, ws, pos, [FILE_W, FILE_H]);
        self.capture_feeds.insert(id, crate::capture::new_slot());
    }

    /// Add a Clipboard capability node (its board starts empty; the local
    /// client pumps the host clipboard through it while an app is wired).
    fn add_clipboard_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.place(id, Kind::Clipboard, ws, pos, [FILE_W, FILE_H]);
        self.clipboard_boards
            .insert(id, crate::clipboard::new_board());
    }

    /// Add a wk API capability node (apps wired to it can drive wk over their
    /// virtual network).
    fn add_api_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.place(id, Kind::Api, ws, pos, [FILE_W, FILE_H]);
    }

    /// Add a HostService node — a host TCP service published into whatever
    /// Network it gets wired to. Defaults target the usual dev server port;
    /// both fields are editable in place.
    fn add_host_service(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.place(id, Kind::HostService, ws, pos, [FILE_W, FILE_H]);
        self.graph.host_services.insert(
            id,
            HostService {
                name: "host".to_string(),
                target: "127.0.0.1:8080".to_string(),
            },
        );
    }

    /// Add a hardware MIDI input node, opening the first available device now.
    fn add_midi_in_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.place(id, Kind::MidiIn, ws, pos, [FILE_W, FILE_H]);
        self.graph.midi_ins.insert(id, String::new());
        self.open_midi_device(id, "");
    }

    /// Point a MidiIn or MidiOut node at a device by name (empty = first
    /// available), (re)opening its hardware connection. The node's kind decides
    /// whether the name names a MIDI source or a MIDI destination.
    fn set_midi_device(&mut self, id: NodeId, want: String) {
        match self.kind_of(id) {
            Some(Kind::MidiIn) => {
                self.graph.midi_ins.insert(id, want.clone());
                self.open_midi_device(id, &want);
            }
            Some(Kind::MidiOut) => {
                self.graph.midi_outs.insert(id, want.clone());
                self.open_midi_out_device(id, &want);
            }
            _ => {} // no other kind has a device
        }
    }

    /// Open (or reopen) the hardware device for MidiIn node `id`, feeding the
    /// MIDI router as `id`. On success the persisted name is set to the resolved
    /// device so it reconnects to the same one; a failure is logged and leaves
    /// the node placed-but-disconnected.
    fn open_midi_device(&mut self, id: NodeId, want: &str) {
        self.midi_devices.remove(&id); // drop any existing connection (closes it)
        match crate::midihw::open(id, want, self.host.midi()) {
            Ok(dev) => {
                self.graph.midi_ins.insert(id, dev.name.clone());
                self.midi_devices.insert(id, dev);
            }
            Err(e) => eprintln!("MIDI input node {id}: {e}"),
        }
    }

    fn remove_midi_in_node(&mut self, id: NodeId) {
        self.midi_devices.remove(&id); // close the device
        self.graph.midi_ins.remove(&id);
        // Drop it from MIDI routing (as a source) and its desired wires.
        self.host.midi().lock().unwrap().remove_node(id);
        self.graph.midi_links.retain(|&(s, d)| s != id && d != id);
        self.routed.retain(|&(s, d)| s != id && d != id);
        self.forget(id);
    }

    /// Add a hardware MIDI output node, opening the first available device now.
    fn add_midi_out_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.place(id, Kind::MidiOut, ws, pos, [FILE_W, FILE_H]);
        self.graph.midi_outs.insert(id, String::new());
        self.open_midi_out_device(id, "");
    }

    /// Open (or reopen) the hardware destination for MidiOut node `id`. On
    /// success the persisted name is set to the resolved device so it
    /// reconnects to the same one; a failure is logged and leaves the node
    /// placed-but-disconnected.
    ///
    /// Reopening gives the node a new inbox, so the routes feeding the old one
    /// would deliver into nothing. They are dropped here and `sync_midi`
    /// rebuilds them against the new inbox on the next tick.
    fn open_midi_out_device(&mut self, id: NodeId, want: &str) {
        self.midi_out_devices.remove(&id); // drop any existing connection
        match crate::midihw::open_output(want) {
            Ok(dev) => {
                self.graph.midi_outs.insert(id, dev.name.clone());
                self.midi_out_devices.insert(id, dev);
            }
            Err(e) => eprintln!("MIDI output node {id}: {e}"),
        }
        let stale: Vec<(NodeId, NodeId)> = self
            .routed
            .iter()
            .copied()
            .filter(|&(_, d)| d == id)
            .collect();
        let router = self.host.midi();
        let mut routes = router.lock().unwrap();
        for (src, dst) in stale {
            routes.disconnect(src, dst);
        }
        drop(routes);
        self.routed.retain(|&(_, d)| d != id);
    }

    fn remove_midi_out_node(&mut self, id: NodeId) {
        self.midi_out_devices.remove(&id); // close the device, stop its pump
        self.graph.midi_outs.remove(&id);
        self.host.midi().lock().unwrap().remove_node(id);
        self.graph.midi_links.retain(|&(s, d)| s != id && d != id);
        self.routed.retain(|&(s, d)| s != id && d != id);
        self.forget(id);
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

    /// Create a Router node at `pos`. It bridges nothing until it is wired to
    /// two or more Networks.
    fn add_router_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.place(id, Kind::Router, ws, pos, [FILE_W, FILE_H]);
        self.sync_routers();
    }

    /// Create an Iroh uplink node at `pos` with a fresh identity.
    fn add_iroh_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.create_uplink(id, None, pos, [FILE_W, FILE_H], ws);
    }

    /// Create a Veilid uplink node at `pos` with a fresh identity.
    fn add_veilid_node(&mut self, pos: [f32; 2], ws: NodeId) {
        let id = self.alloc_id();
        self.create_veilid_uplink(id, None, pos, [FILE_W, FILE_H], ws);
    }

    /// Create (or restore) a Veilid uplink node with a known id (and, when
    /// restoring, its persisted DHT owner keypair, so its ticket is unchanged).
    fn create_veilid_uplink(
        &mut self,
        id: NodeId,
        identity: Option<&str>,
        pos: [f32; 2],
        size: [f32; 2],
        ws: NodeId,
    ) {
        match self.host.veilid_uplink(id, identity, id) {
            Ok(up) => {
                eprintln!("[veilid] uplink {id} ticket: {}", up.ticket());
                self.graph.veilid_ids.insert(id, up.identity().to_string());
                self.uplinks.insert(id, UplinkHandle::Veilid(up));
                self.place(id, Kind::Veilid, ws, pos, size);
            }
            Err(e) => eprintln!("failed to start veilid uplink: {e:#}"),
        }
    }

    /// Create (or restore) an Iroh uplink node with a known id (and, when
    /// restoring, its persisted secret, so its ticket is unchanged). Until
    /// wired to a Network the uplink trunks the node's own (empty) net, so a
    /// connected peer sees nothing.
    fn create_uplink(
        &mut self,
        id: NodeId,
        secret: Option<[u8; 32]>,
        pos: [f32; 2],
        size: [f32; 2],
        ws: NodeId,
    ) {
        match self.host.uplink(id, secret) {
            Ok(up) => {
                eprintln!("[iroh] uplink {id} ticket: {}", up.ticket());
                self.graph.iroh_secrets.insert(id, up.secret());
                self.uplinks.insert(id, UplinkHandle::Iroh(up));
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
                .node_deps
                .get(&id)
                .and_then(|want| self.graph.available.iter().find(|d| &d.name == want))
                .cloned()
            else {
                return;
            };
            let args = self
                .graph
                .node_args
                .get(&id)
                .cloned()
                .unwrap_or_else(|| dep.effective_args());
            let options = node.options.lock().unwrap().clone();
            let new_id = self.alloc_id();
            let name = self.generated_node_name(new_id);
            if let Err(e) = self.host.spawn(
                &dep.local_path(),
                &name,
                new_id,
                &args,
                self.registry.clone(),
                self.node_reg.clone(),
                options,
                dep.container(),
            ) {
                eprintln!("failed to duplicate {}: {e:#}", dep.name);
                return;
            }
            self.place(new_id, Kind::App, ws, off, size);
            self.graph.node_deps.insert(new_id, dep.name.clone());
            self.graph.node_names.insert(new_id, name);
            self.graph.node_args.insert(new_id, args);
            return;
        }

        match self
            .graph
            .file_nodes
            .get(&id)
            .map(|f| matches!(f, FileNode::Volume(_)))
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
            Some(Kind::Veilid) => self.add_veilid_node(off, ws),
            // A second instance of the same definition, wired the same way. It
            // takes a new id, so its nodes derive afresh and the two copies
            // share nothing — which is the whole point of instancing.
            Some(Kind::Group) => {
                if let Some(g) = self.graph.groups.get(&id).cloned() {
                    let new_id = self.alloc_id();
                    self.place(new_id, Kind::Group, ws, off, size);
                    self.graph.groups.insert(new_id, g);
                    self.expand_group(ws, new_id);
                }
            }
            _ => {}
        }
    }

    /// Remove a node by kind (app/file/port/network/uplink).
    fn remove_any(&mut self, id: NodeId) {
        match self.kind_of(id) {
            Some(Kind::File) => self.remove_file_node(id),
            Some(Kind::Port) => self.remove_host_port(id),
            Some(Kind::Network | Kind::Gateway) => self.remove_net_node(id),
            Some(Kind::Iroh | Kind::Veilid) => self.remove_uplink_node(id),
            Some(Kind::App) => self.close_node(id),
            // A note wires to nothing and runs nothing; just drop it.
            Some(Kind::Note) => self.forget(id),
            Some(Kind::Capture) => self.remove_capture_node(id),
            Some(Kind::Clipboard) => self.remove_clipboard_node(id),
            Some(Kind::Api) => self.remove_api_node(id),
            Some(Kind::MidiIn) => self.remove_midi_in_node(id),
            Some(Kind::MidiOut) => self.remove_midi_out_node(id),
            // Its net wire lives in net_links with the service as the "member"
            // side; forget() kills the listener and drops the config.
            Some(Kind::HostService) => {
                self.graph.net_links.retain(|&(svc, _)| svc != id);
                self.forget(id);
            }
            Some(Kind::Router) => {
                self.graph.net_links.retain(|&(m, _)| m != id);
                self.forget(id);
                self.sync_routers();
            }
            Some(Kind::Boundary) => self.remove_boundary_port(id),
            Some(Kind::Group) => self.remove_group(id),
            None => {}
        }
    }

    /// Remove a boundary port. It has no runtime effect to unwind — nothing was
    /// ever mounted, routed or granted through it — but its wires live in the
    /// ordinary relations, so they go with it rather than dangling.
    fn remove_boundary_port(&mut self, id: NodeId) {
        let untouched = |&(a, b): &(NodeId, NodeId)| a != id && b != id;
        self.graph.connections.retain(untouched);
        self.graph.midi_links.retain(untouched);
        self.graph.capture_links.retain(untouched);
        self.graph.clipboard_links.retain(untouched);
        self.graph.api_links.retain(untouched);
        prune_side_map(&mut self.graph.mount_paths, &self.graph.connections);
        self.forget(id);
    }

    /// Remove a Capture node: revoke every grant through it, drop its feed.
    fn remove_capture_node(&mut self, id: NodeId) {
        self.graph.capture_links.retain(|&(_, cap)| cap != id);
        self.capture_feeds.remove(&id);
        self.sync_captures();
        self.forget(id);
    }

    /// Toggle a screen-capture grant (one capture source per app), then point
    /// wired apps at their granted frame slots.
    fn toggle_capture(&mut self, app: NodeId, cap: NodeId) {
        wiring::toggle_unique(&mut self.graph.capture_links, app, cap);
        self.sync_captures();
    }

    /// Remove a Clipboard node: revoke every grant through it, drop its board.
    fn remove_clipboard_node(&mut self, id: NodeId) {
        self.graph.clipboard_links.retain(|&(_, clip)| clip != id);
        self.clipboard_boards.remove(&id);
        self.sync_clipboard();
        self.forget(id);
    }

    /// Toggle a host-clipboard grant (one Clipboard node per app), then point
    /// wired apps at their granted board and refresh their permits.
    fn toggle_clipboard(&mut self, app: NodeId, clip: NodeId) {
        wiring::toggle_unique(&mut self.graph.clipboard_links, app, clip);
        self.sync_clipboard();
    }

    /// Remove an Api node: revoke every grant through it.
    fn remove_api_node(&mut self, id: NodeId) {
        self.graph.api_links.retain(|&(_, api)| api != id);
        self.forget(id);
    }

    /// Toggle an API grant (one API node per app).
    fn toggle_api(&mut self, app: NodeId, api: NodeId) {
        wiring::toggle_unique(&mut self.graph.api_links, app, api);
        // effect reconciled by sync_apis (added separately)
    }

    /// Point every app node's `capture_src` at its granted Capture node's frame
    /// slot (or clear it). Mirrors `sync_midi`: the graph's capture links are
    /// the desired state; this makes the runtime match.
    /// Refresh every app node's `wk:exec` permission from its capability
    /// token. Unlike the wired capabilities this needs no wire — running a
    /// program from your own filesystem gains no authority (the child inherits
    /// the caller's vfs and nothing else) — but it is still a token decision,
    /// so attenuating `exec` away stops further runs within a tick.
    fn sync_exec(&mut self) {
        let nodes: Vec<crate::plugin::SharedNode> = self.node_reg.lock().unwrap().clone();
        for node in nodes {
            let allowed = self.node_may_use(node.id, "exec", node.id, "spawn");
            node.exec_permit
                .store(allowed, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn sync_captures(&mut self) {
        let nodes: Vec<crate::plugin::SharedNode> = self.node_reg.lock().unwrap().clone();
        for node in nodes {
            let pair = self
                .graph
                .capture_links
                .iter()
                .find(|&&(app, _)| app == node.id)
                .copied();
            let feed = pair.and_then(|(app, cap)| {
                if self.node_may_use(app, "capture", cap, "read") {
                    self.capture_feeds.get(&cap).cloned()
                } else {
                    None
                }
            });
            *node.capture_src.lock().unwrap() = feed;
        }
    }

    /// Point every app node's `clip_src` at its granted Clipboard node's board
    /// (or clear it) and refresh its two permits from its capability token.
    ///
    /// TWO `node_may_use` calls, not one. `clipboard`/`read` and
    /// `clipboard`/`write` are independent actions on the same wire — the same
    /// split `sync_mounts` uses for `file` read/write, rather than the
    /// by-direction split `midi` uses, because an app↔Clipboard wire has no
    /// direction. That is what makes
    ///
    /// ```text
    /// wk token attenuate <node> \
    ///   'check if operation($k,$t,$a), $k != "clipboard" || $a == "write"'
    /// ```
    ///
    /// mean "may copy out, may never see what the user copied elsewhere".
    ///
    /// The board is attached when EITHER action is allowed and cleared when
    /// neither is, so a node denied both cannot even tell a Clipboard node is
    /// there. Runs every tick, so revocation is live: the auth cache is keyed
    /// on the token+wires fingerprint, so a `wk token attenuate` takes effect
    /// on the next pass without restarting the guest.
    fn sync_clipboard(&mut self) {
        let nodes: Vec<crate::plugin::SharedNode> = self.node_reg.lock().unwrap().clone();
        for node in nodes {
            let pair = self
                .graph
                .clipboard_links
                .iter()
                .find(|&&(app, _)| app == node.id)
                .copied();
            let (read, write) = match pair {
                Some((app, clip)) => (
                    self.node_may_use(app, "clipboard", clip, "read"),
                    self.node_may_use(app, "clipboard", clip, "write"),
                ),
                None => (false, false),
            };
            node.clip_read
                .store(read, std::sync::atomic::Ordering::Relaxed);
            node.clip_write
                .store(write, std::sync::atomic::Ordering::Relaxed);
            let board = pair
                .filter(|_| read || write)
                .and_then(|(_, clip)| self.clipboard_boards.get(&clip).cloned());
            *node.clip_src.lock().unwrap() = board;
        }
    }

    /// Delete a workspace and every node in it. A no-op for the last workspace —
    /// a document always keeps at least one.
    ///
    /// `graph.workspaces` holds only tabs, so the "keep at least one" guard
    /// counts tabs and an instance can never be closed as if it were one. The
    /// nodes an instance placed are not in this tab (their workspace *is* the
    /// instance) — they go with the `group` node that stands for them, which is.
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
        self.graph.workspace_names.remove(&id);
    }

    /// (Re)run an idle or exited node's guest with its current args.
    fn run_node(&mut self, id: NodeId) {
        let Some(node) = self.app_node(id) else {
            return;
        };
        // Still compiling? Remember the intent and start it the moment its
        // component (and container rootfs) is ready — the tick loop drains
        // `pending_run`. Clicking play on a big container mid-compile must not be
        // a lost no-op: `host.run_node` would silently do nothing here.
        if node.is_loading() {
            self.pending_run.insert(id);
            return;
        }
        self.pending_run.remove(&id);
        // A container's rootfs is mounted on the compile thread (see
        // `PluginHost::spawn`) — done by the time the node stops loading — so a
        // bind mounted at load would be shadowed by it. Re-lay this app's binds
        // now, on top of the rootfs, right before the guest reads its filesystem.
        self.reapply_binds(id);
        let args = self.graph.node_args.get(&id).cloned().unwrap_or_default();
        if let Err(e) = self.host.run_node(&node, &args) {
            eprintln!("failed to run {}: {e:#}", node.name);
        }
    }

    /// Start any node whose run was requested while it was still compiling, now
    /// that it's ready. Drops entries whose node vanished or failed to compile.
    fn drain_pending_run(&mut self) {
        if self.pending_run.is_empty() {
            return;
        }
        for id in self.pending_run.iter().copied().collect::<Vec<_>>() {
            match self.app_node(id) {
                // Still compiling — keep waiting, unless the compile failed (setup
                // never published but the thread finished), in which case give up.
                Some(node) if node.is_loading() => {
                    if node.finished.load(Ordering::Relaxed) {
                        self.pending_run.remove(&id);
                    }
                }
                Some(_) => self.run_node(id), // ready → run (clears pending)
                None => {
                    self.pending_run.remove(&id); // node gone
                }
            }
        }
    }

    /// Re-mount every bind wired into app `id` at its current path, over whatever
    /// is already in the app's fs (its container rootfs). Idempotent: unmounts
    /// any stale mount for the pair first. Ensures a guest sees its mounts even
    /// when the bind was recorded before the async rootfs mount clobbered it.
    fn reapply_binds(&mut self, app: NodeId) {
        let Some(node) = self.app_node(app) else {
            return;
        };
        let files: Vec<NodeId> = self
            .graph
            .connections
            .iter()
            .filter(|(_, a)| *a == app)
            .map(|(f, _)| *f)
            .collect();
        for file in files {
            let pair = (file, app);
            let path = self.mount_path_for(file, app);
            if let Some((old, fs, _)) = self.mounted.remove(&pair) {
                crate::vfs::unmount_file(&fs, &old);
            }
            // Same token gates as sync_mounts — re-applying must not resurrect
            // a denied mount (or upgrade a read-only one).
            if !self.node_may_use(app, "file", file, "read") {
                continue;
            }
            let writable = self.node_may_use(app, "file", file, "write");
            if let Some(f) = self.graph.file_nodes.get(&file) {
                f.mount(&node.fs, &path, writable);
                self.mounted.insert(pair, (path, node.fs.clone(), writable));
            }
        }
        // A container rootfs may have just landed over /run — republish.
        self.write_token_file(app);
    }

    /// Stop a running app node's guest without removing the node: halt the
    /// guest but leave the node, its wiring, and its net stack in place, so
    /// `Run` can re-spawn it (run_node resets the kill/finished flags).
    fn stop_node(&mut self, id: NodeId) {
        self.halt_guest(id);
    }

    /// Signal a node's guest to exit and wake it if it's parked. Sets the kill
    /// switch (tripped at the next epoch — within ~100ms for a terminal node,
    /// whose stdin poll is capped) and closes+removes any surface it owns, so a
    /// guest blocked on `frame.block()` wakes and exits (and no stale surface
    /// lingers). Shared by `stop_node` and `close_node`; leaves the node record
    /// and its terminal/net state untouched so it can be re-run.
    fn halt_guest(&self, id: NodeId) {
        if let Some(node) = self.app_node(id) {
            node.kill.store(true, Ordering::Relaxed);
            // EOF on stdin so a guest parked in a plain blocking read (no tty
            // poll cap) wakes and exits; run_node reopens it on restart.
            node.term_io.close();
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
    }

    /// Set a node's launch args from a whitespace-separated string. Guarded to
    /// existing nodes so an `Update` on an unknown id can't grow `node_args`
    /// without bound. For an uplink the args are its peer ticket — this dials
    /// it live (or *undials*, stopping the dialer, when cleared), so undo and
    /// clearing stay in sync with the running uplink instead of diverging.
    fn set_node_args(&mut self, id: NodeId, text: &str) {
        if !self.graph.nodes.contains_key(&id) {
            return;
        }
        let args = text.split_whitespace().map(str::to_string).collect();
        self.graph.node_args.insert(id, args);
        if let Some(up) = self.uplinks.get(&id) {
            // Empty ticket → undial (Uplink::dial treats "" as "stop dialing").
            if let Err(e) = up.dial(text.trim()) {
                eprintln!("[uplink] {e:#}");
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
            Some(Kind::Router) => NodeClass::Router,
            Some(Kind::Iroh | Kind::Veilid) => NodeClass::Uplink,
            Some(Kind::Capture) => NodeClass::Capture,
            Some(Kind::Clipboard) => NodeClass::Clipboard,
            Some(Kind::Api) => NodeClass::Api,
            Some(Kind::MidiIn) => NodeClass::MidiSource,
            Some(Kind::MidiOut) => NodeClass::MidiSink,
            Some(Kind::HostService) => NodeClass::HostSvc,
            // A boundary port's class is *declared*, not inferred: it comes
            // from the side table placed alongside the node record.
            Some(Kind::Boundary) => self
                .graph
                .boundary_ports
                .get(&id)
                .map_or(NodeClass::Other, |p| NodeClass::Boundary(p.dir, p.kind)),
            Some(Kind::Group) => NodeClass::Instance,
            Some(Kind::App) | Some(Kind::Note) | None => NodeClass::Other,
        }
    }

    /// Toggle a connection between two nodes by their kinds: file⇄app mounts the
    /// file; http-app⇄HostPort serves on localhost; app⇄Network joins the network;
    /// app⇄app wires MIDI. The *decision* (which wire, which orientation) is
    /// [`wiring::classify`]; this only runs the effect for whichever it returns.
    fn connect_toggle(&mut self, a: NodeId, b: NodeId) {
        match wiring::classify(a, b, self.class_of(a), self.class_of(b)) {
            Some(Wire::Bind(file, app)) => self.toggle_file(file, app),
            Some(Wire::Serve(http, hostport)) => self.toggle_serve(http, hostport),
            Some(Wire::Net(app, net)) => self.toggle_net(app, net),
            Some(Wire::Capture(app, cap)) => self.toggle_capture(app, cap),
            Some(Wire::Clipboard(app, clip)) => self.toggle_clipboard(app, clip),
            Some(Wire::Api(app, api)) => self.toggle_api(app, api),
            Some(Wire::Midi(src, dst)) => {
                // Two apps normally wire MIDI — but if one of them serves a
                // filesystem (imports `wk:fs/provider`), the wire is a mount:
                // the provider's tree appears in the other app's vfs, exactly
                // like a Volume bind (same relation, same tokens, same
                // per-connection mount path). While either endpoint is still
                // compiling that distinction is unknowable, so the wire waits
                // in `pending_app_wires` for the tick reconciler.
                let loading = |s: &Self, id: NodeId| s.app_node(id).is_some_and(|n| n.is_loading());
                if loading(self, src) || loading(self, dst) {
                    self.pending_app_wires.push((src, dst));
                } else if self.app_node(src).is_some_and(|n| n.serves_fs()) {
                    self.toggle_file(src, dst)
                } else if self.app_node(dst).is_some_and(|n| n.serves_fs()) {
                    self.toggle_file(dst, src)
                } else {
                    self.toggle_midi(src, dst)
                }
            }
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
        let net_kind = if self.is_gateway(net_id) {
            "gateway"
        } else {
            "net"
        };
        // A router is the one member that may be on several networks at once —
        // bridging them is what it is — so its wire is a plain pair rather than
        // the one-per-source toggle every other member uses.
        if self.kind_of(app_id) == Some(Kind::Router) {
            wiring::toggle_pair(&mut self.graph.net_links, app_id, net_id);
            self.sync_routers();
            return;
        }
        let joined = wiring::toggle_unique(&mut self.graph.net_links, app_id, net_id)
            // The immediate join is gated on the member's token too, not just
            // the per-tick sync — no one-tick window on the network.
            && self.node_may_use(app_id, net_kind, net_id, "use");
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
    /// its membership applied here once it's ready. A member whose token does
    /// not allow the net (denied, or the authorization was revoked) is held in
    /// isolation instead — the wire stays on the canvas but grants nothing.
    fn sync_net_membership(&mut self) {
        let nodes = self.node_reg.lock().unwrap().clone();
        let links = self.graph.net_links.clone();
        for (app, net) in links {
            let kind = if self.is_gateway(net) {
                "gateway"
            } else {
                "net"
            };
            let allowed = self.node_may_use(app, kind, net, "use");
            if let Some(up) = self.uplinks.get(&app) {
                up.set_net(if allowed { net } else { app });
                continue;
            }
            let Some(stack) = nodes
                .iter()
                .find(|n| n.id == app)
                .and_then(|n| n.net_stack())
            else {
                continue;
            };
            let (want_net, want_host) = if allowed {
                (net, self.is_gateway(net))
            } else {
                (app, false) // own empty net = isolated
            };
            let mut g = stack.lock().unwrap();
            if g.net != want_net || g.host_access != want_host {
                g.net = want_net;
                g.host_access = want_host;
            }
        }
    }

    /// Re-publish every router's networks to the fabric. A router holds one
    /// hub-side port for as long as it is wired to anything; the port's net
    /// list is the wires it currently has, filtered by what its token allows,
    /// so attenuating a router closes the bridge on the next tick.
    fn sync_routers(&mut self) {
        let ids: Vec<NodeId> = self
            .graph
            .nodes
            .iter()
            .filter(|(_, rec)| rec.kind == Kind::Router)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            let wired: Vec<NodeId> = self
                .graph
                .net_links
                .iter()
                .filter(|&&(member, _)| member == id)
                .map(|&(_, net)| net)
                .collect();
            let nets: Vec<NodeId> = wired
                .into_iter()
                .filter(|&net| {
                    let kind = if self.is_gateway(net) {
                        "gateway"
                    } else {
                        "net"
                    };
                    self.node_may_use(id, kind, net, "use")
                })
                .collect();
            match self.routers.get(&id) {
                // Bridging fewer than two networks is not a bridge; drop the
                // port so a half-wired router cannot leak anything.
                _ if nets.len() < 2 => {
                    if let Some(r) = self.routers.remove(&id) {
                        self.host.hub().detach_router(&r);
                    }
                }
                Some(r) => r.set_nets(nets),
                None => {
                    let r = self.host.hub().attach_router(nets);
                    self.routers.insert(id, r);
                }
            }
        }
        // A router that no longer exists takes its bridge with it.
        let gone: Vec<NodeId> = self
            .routers
            .keys()
            .copied()
            .filter(|id| self.kind_of(*id) != Some(Kind::Router))
            .collect();
        for id in gone {
            if let Some(r) = self.routers.remove(&id) {
                self.host.hub().detach_router(&r);
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

    /// Remove an uplink node (Iroh or Veilid); dropping the uplink closes its
    /// endpoint and detaches its trunk from the fabric.
    fn remove_uplink_node(&mut self, id: NodeId) {
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

    /// The guest (container) port a serve wire forwards to: the per-wire mapping
    /// if set, else the HostPort's own `host_port` (forward verbatim).
    fn serve_port_for(&self, served: NodeId, hostport: NodeId, host_port: u16) -> u16 {
        self.graph
            .serve_ports
            .get(&(served, hostport))
            .copied()
            .unwrap_or(host_port)
    }

    /// Set (or clear) the guest port a serve wire maps to, rebinding it live. A
    /// `0`/absent container port resets to the HostPort's own port.
    fn set_serve_port(&mut self, served: NodeId, hostport: NodeId, container: u16) {
        if !self.graph.serve_links.contains(&(served, hostport)) {
            return; // not a serve wire
        }
        if container == 0 {
            self.graph.serve_ports.remove(&(served, hostport));
        } else {
            self.graph.serve_ports.insert((served, hostport), container);
        }
        // Rebind so the new mapping takes effect: stop the running server, then
        // let the next reconcile (or this call) restart it.
        if let Some((_, kill)) = self.serves.remove(&served) {
            kill.store(true, Ordering::Relaxed);
        }
        self.sync_serves();
    }

    /// Reconcile the running [`Self::serves`] against the desired
    /// [`Self::serve_links`]: stop servers whose wiring changed or whose node/port
    /// went away, and start desired servers that aren't running yet and are now
    /// ready. Idempotent and cheap when nothing changed; called after any serve
    /// change and once per tick (so a wire made before its node finished
    /// compiling is honored as soon as the node comes up).
    /// Install the callback that serves accepted fabric API connections.
    pub fn set_api_conn_server(&mut self, f: ApiConnServer) {
        self.api_conn_server = Some(f);
    }

    /// A node's effective capability token bytes: its custom token, else the
    /// workspace base — empty when node auth is off (allow-all posture; with
    /// no key configured the API side also verifies nothing).
    fn effective_node_token(&self, id: NodeId) -> Vec<u8> {
        self.graph
            .node_tokens
            .get(&id)
            .cloned()
            .or_else(|| self.node_auth.as_ref().map(|(_, base)| base.clone()))
            .unwrap_or_default()
    }

    /// Publish a node's effective capability token into its own filesystem at
    /// `/run/wk/token` (hex), so a guest can read the credential it acts
    /// under — to attenuate it offline, or to present it to a *remote* wk.
    /// (Calls over the local Api wire don't need it: those connections
    /// implicitly bear this token.) Refreshed on every token change.
    fn write_token_file(&self, id: NodeId) {
        let Some(node) = self.app_node(id) else {
            return;
        };
        let token = self.effective_node_token(id);
        if token.is_empty() {
            return; // no auth configured — nothing meaningful to publish
        }
        let hex = crate::workspace::bytes_hex(&token);
        node.fs
            .lock()
            .unwrap()
            .put_file_at("run/wk/token", hex.into_bytes());
    }

    /// Reconcile fabric API endpoints against the desired `api_links`: a node
    /// wired to an Api node (whose token allows the wire) gets a named `api`
    /// peer on its network, serving the wk protocol on
    /// [`wk_protocol::API_PORT`]; each accepted connection bears the node's
    /// own token. A cut/denied wire kills the endpoint; a token swap restarts
    /// it so new connections carry the new token (live ones keep the old —
    /// the same session semantics as any bearer connection).
    fn sync_apis(&mut self) {
        use std::hash::{Hash, Hasher};
        let links = self.graph.api_links.clone();
        let mut desired: HashMap<NodeId, (NodeId, u64, Vec<u8>, wk_fabric::netstack::SharedStack)> =
            HashMap::new();
        for (app, api) in links {
            let Some(stack) = self.app_node(app).and_then(|n| n.net_stack()) else {
                continue; // not compiled yet (or no wasi:sockets) — retried next tick
            };
            if !self.node_may_use(app, "api", api, "use") {
                continue;
            }
            let token = self.effective_node_token(app);
            let fp = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                token.hash(&mut h);
                h.finish()
            };
            desired.insert(app, (api, fp, token, stack));
        }
        // Stop endpoints that are stale: unwired/denied, rewired to another Api
        // node, or started under a different token.
        let stale: Vec<NodeId> = self
            .api_serves
            .iter()
            .filter(|(app, (api, _, fp))| {
                desired
                    .get(app)
                    .map(|(dapi, dfp, _, _)| dapi != api || dfp != fp)
                    .unwrap_or(true)
            })
            .map(|(&app, _)| app)
            .collect();
        for app in stale {
            if let Some((_, kill, _)) = self.api_serves.remove(&app) {
                kill.store(true, Ordering::Relaxed);
            }
        }
        // Start the missing ones.
        for (app, (api, fp, token, stack)) in desired {
            if self.api_serves.contains_key(&app) {
                continue;
            }
            let Some(serve) = self.api_conn_server.clone() else {
                continue;
            };
            let kill = Arc::new(AtomicBool::new(false));
            let on_conn: Arc<dyn Fn(std::os::unix::net::UnixStream) + Send + Sync> =
                Arc::new(move |stream| serve(stream, token.clone()));
            wk_fabric::listen::listen(
                self.host.hub(),
                stack,
                "api",
                wk_protocol::API_PORT,
                kill.clone(),
                on_conn,
            );
            self.api_serves.insert(app, (api, kill, fp));
        }
    }

    /// Reconcile HostService fabric listeners against the desired wiring: one
    /// named listener on the wired Network per service node, each accepted
    /// connection bridged to the node's host `addr:port`. Mirrors `sync_apis`
    /// (desired-set diff, kill-switch teardown, fingerprint restart), but
    /// scoped to a *network* rather than following one node — any member may
    /// dial, because the endpoint carries no caller authority.
    fn sync_host_services(&mut self) {
        use std::hash::{Hash, Hasher};
        let mut desired: HashMap<NodeId, (NodeId, u64, HostService)> = HashMap::new();
        for &(svc, net) in &self.graph.net_links {
            let Some(cfg) = self.graph.host_services.get(&svc) else {
                continue; // not a HostService wire
            };
            if cfg.port().is_none() {
                continue; // target doesn't parse yet — nothing to publish
            }
            let fp = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                net.hash(&mut h);
                cfg.name.hash(&mut h);
                cfg.target.hash(&mut h);
                h.finish()
            };
            desired.insert(svc, (net, fp, cfg.clone()));
        }
        let stale: Vec<NodeId> = self
            .host_service_serves
            .iter()
            .filter(|(svc, (_, fp))| {
                desired
                    .get(svc)
                    .map(|(_, dfp, _)| dfp != fp)
                    .unwrap_or(true)
            })
            .map(|(&svc, _)| svc)
            .collect();
        for svc in stale {
            if let Some((kill, _)) = self.host_service_serves.remove(&svc) {
                kill.store(true, Ordering::Relaxed);
            }
        }
        for (svc, (net, fp, cfg)) in desired {
            if self.host_service_serves.contains_key(&svc) {
                continue;
            }
            let port = cfg.port().expect("filtered above");
            let target = cfg.target.clone();
            let kill = Arc::new(AtomicBool::new(false));
            let on_conn: Arc<dyn Fn(std::os::unix::net::UnixStream) + Send + Sync> =
                Arc::new(move |stream| bridge_to_host(stream, target.clone()));
            wk_fabric::listen::listen_net(
                self.host.hub(),
                net,
                &cfg.name,
                port,
                kill.clone(),
                on_conn,
                // The same target serves both protocols, so a published
                // service that speaks UDP (DNS, QUIC, a game server) works
                // like the TCP one rather than silently blackholing.
                Some(cfg.target.clone()),
            );
            self.host_service_serves.insert(svc, (kill, fp));
        }
    }

    fn sync_serves(&mut self) {
        // Which serve links are serviceable *right now*: a wasi:http node can be
        // served as soon as it's compiled (its handler is invoked per request);
        // a fabric (wasi:sockets) node only once its guest is actually running and
        // thus listening. Gating on `running` means a HostPort publishes exactly
        // when its node is live — never dialing a not-yet-listening guest, and it
        // unbinds automatically when the node stops or exits. The node's token
        // must also allow the publish; a denied/revoked one unbinds the same way.
        let links = self.graph.serve_links.clone();
        let mut desired: HashMap<NodeId, NodeId> = HashMap::new();
        for (http, hp) in links {
            let ready = self.app_node(http).is_some_and(|n| {
                n.http_path().is_some()
                    || (n.net_stack().is_some() && n.running.load(Ordering::Relaxed))
            });
            if ready && self.node_may_use(http, "port", hp, "use") {
                desired.insert(http, hp);
            }
        }
        // Stop forwards no longer desired: the wire was cut, the node stopped or
        // exited, or it's now published on a different HostPort.
        let stale: Vec<NodeId> = self
            .serves
            .iter()
            .filter(|(http, (hp, _))| desired.get(http) != Some(hp))
            .map(|(&http, _)| http)
            .collect();
        for http in stale {
            if let Some((hostport, kill)) = self.serves.remove(&http) {
                kill.store(true, Ordering::Relaxed);
                self.port_errors.remove(&hostport);
            }
        }
        // (Re)bind the newly-serviceable ones. `start_serve` applies its own
        // port-conflict guard and records any bind failure.
        for (&http, &hostport) in &desired {
            if !self.serves.contains_key(&http) {
                self.start_serve(http, hostport);
            }
        }
        // Drop container-port mappings for serve wires that no longer exist, and
        // any bind-error against a HostPort no longer on the receiving end of one.
        prune_side_map(&mut self.graph.serve_ports, &self.graph.serve_links);
        let served: HashSet<NodeId> = self.graph.serve_links.iter().map(|&(_, hp)| hp).collect();
        self.port_errors.retain(|hp, _| served.contains(hp));
    }

    /// Try to bind the server for one desired serve link. A wasi:http node gets
    /// an HTTP server dispatching into its handler; a fabric (wasi:sockets) node
    /// gets a TCP+UDP forward from the localhost port to its fabric address at
    /// the wire's mapped guest port (`host:container`, the same by default).
    /// Silently does nothing if the node isn't ready yet or its port is already
    /// served (both are transient during async compile / port conflicts); only a
    /// real bind failure is logged.
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
            // host:container — forward the localhost port to the guest's own port
            // (defaults to the same number when no mapping is set).
            let guest_port = self.serve_port_for(http_id, hostport_id, port);
            self.host.forward(stack, port, guest_port, kill.clone())
        } else {
            return; // still compiling, or a node with nothing to serve
        };
        if let Err(e) = bound {
            // Most often the localhost port is already taken (by another process
            // — Docker, another server — or a stale wk). Record it against the
            // HostPort so the client can warn, and log the detail once.
            eprintln!("failed to serve {} on :{port}: {e:#}", node.name);
            self.port_errors
                .insert(hostport_id, format!("localhost:{port} unavailable: {e}"));
            return;
        }
        self.port_errors.remove(&hostport_id);
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
    /// Nudge a HostPort's localhost port by `delta` (the GUI's −/+ buttons).
    fn change_port(&mut self, id: NodeId, delta: i32) {
        if let Some(&cur) = self.graph.host_ports.get(&id) {
            self.set_host_port(id, (cur as i32 + delta).clamp(1, 65535) as u16);
        }
    }

    /// Set a HostPort's localhost port absolutely, live-rebinding any server.
    fn set_host_port(&mut self, id: NodeId, new: u16) {
        let Some(&cur) = self.graph.host_ports.get(&id) else {
            return; // not a HostPort node
        };
        if new == cur || new == 0 {
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

    /// The in-app path a bind mounts at: the per-connection override if set,
    /// else the volume's own name at the filesystem root (the default).
    fn mount_path_for(&self, volume: NodeId, app: NodeId) -> String {
        self.graph
            .mount_paths
            .get(&(volume, app))
            .cloned()
            .unwrap_or_else(|| {
                self.graph
                    .file_nodes
                    .get(&volume)
                    .map(|f| f.name().to_string())
                    // A provider app's tree mounts under the app's name by
                    // default, like a volume mounts under its own.
                    .or_else(|| self.app_node(volume).map(|n| n.name.clone()))
                    .unwrap_or_default()
            })
    }

    /// Turn on node-capability enforcement: `public_key` verifies node tokens
    /// (a copy of the token service's key), `base_token` is the default token
    /// every app node holds (see `wk_token_service::mint_node_base`). Until
    /// this is called, wiring effects apply unconditionally.
    pub fn set_node_auth(&mut self, public_key: biscuit_auth::PublicKey, base_token: Vec<u8>) {
        // Tokens restored from the .wk before auth was configured couldn't be
        // verified then; drop any that this service didn't sign (the key file
        // was lost, or the file came from another machine) so those nodes fall
        // back to the default token instead of being denied everything.
        self.graph.node_tokens.retain(|id, t| {
            let ok = biscuit_auth::Biscuit::from(t, public_key).is_ok();
            if !ok {
                eprintln!(
                    "wk: node {id} has a token from a different token service; \
                     using the default"
                );
            }
            ok
        });
        self.node_auth = Some((public_key, base_token));
        self.auth_cache.clear();
        // Load-time nodes spawned before auth existed get their token file now.
        let apps: Vec<NodeId> = self
            .graph
            .nodes
            .iter()
            .filter(|(_, r)| matches!(r.kind, Kind::App))
            .map(|(&id, _)| id)
            .collect();
        for id in apps {
            self.write_token_file(id);
        }
    }

    /// The token service's public key, if auth is configured — what a client
    /// connection verifies its bearer token against for the read path.
    pub fn auth_public_key(&self) -> Option<biscuit_auth::PublicKey> {
        self.node_auth.as_ref().map(|(k, _)| *k)
    }

    /// Every wire app node `id` currently has, as `(kind, counterpart)` — the
    /// ambient `wired(...)` facts its token's Datalog runs against.
    fn wires_of(&self, id: NodeId) -> Vec<(&'static str, NodeId)> {
        let g = &self.graph;
        let mut w: Vec<(&'static str, NodeId)> = Vec::new();
        w.extend(
            g.connections
                .iter()
                .filter(|&&(_, a)| a == id)
                .map(|&(f, _)| ("file", f)),
        );
        // A MIDI wire is a fact for both endpoints (source and destination).
        w.extend(
            g.midi_links
                .iter()
                .filter(|&&(s, _)| s == id)
                .map(|&(_, d)| ("midi", d)),
        );
        w.extend(
            g.midi_links
                .iter()
                .filter(|&&(_, d)| d == id)
                .map(|&(s, _)| ("midi", s)),
        );
        w.extend(
            g.serve_links
                .iter()
                .filter(|&&(h, _)| h == id)
                .map(|&(_, hp)| ("port", hp)),
        );
        // A gateway grants host access — a different capability than a plain
        // net, so it gets its own kind (Datalog has no negation; cutting off
        // host access must be expressible by kind matching).
        w.extend(
            g.net_links
                .iter()
                .filter(|&&(a, _)| a == id)
                .map(|&(_, n)| {
                    if g.nodes.get(&n).map(|r| r.kind) == Some(Kind::Gateway) {
                        ("gateway", n)
                    } else {
                        ("net", n)
                    }
                }),
        );
        w.extend(
            g.capture_links
                .iter()
                .filter(|&&(a, _)| a == id)
                .map(|&(_, c)| ("capture", c)),
        );
        w.extend(
            g.clipboard_links
                .iter()
                .filter(|&&(a, _)| a == id)
                .map(|&(_, c)| ("clipboard", c)),
        );
        w.extend(
            g.api_links
                .iter()
                .filter(|&&(a, _)| a == id)
                .map(|&(_, n)| ("api", n)),
        );
        w
    }

    /// Whether node `id`'s capability token authorizes `action` on
    /// `(kind, target)`. Always true with enforcement off, and for non-app
    /// nodes (uplinks, hardware sources — those are resources/host-owned, not
    /// token-bearing subjects). Decisions are memoized against a fingerprint of
    /// the token bytes + the node's wire set, so the per-tick reconcilers stay
    /// cheap.
    fn node_may_use(
        &mut self,
        id: NodeId,
        kind: &'static str,
        target: NodeId,
        action: &'static str,
    ) -> bool {
        if self.node_auth.is_none() {
            return true;
        }
        if !matches!(self.graph.nodes.get(&id).map(|r| r.kind), Some(Kind::App)) {
            return true;
        }
        let wires = self.wires_of(id);
        let (key, base) = self.node_auth.as_ref().unwrap();
        let key = *key;
        let token = self.graph.node_tokens.get(&id).unwrap_or(base);
        let fp = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            token.hash(&mut h);
            wires.hash(&mut h);
            h.finish()
        };
        if let Some(&(cached_fp, ok)) = self.auth_cache.get(&(id, kind, target, action)) {
            if cached_fp == fp {
                return ok;
            }
        }
        let token = token.clone();
        let facts: Vec<(&str, String)> = wires.iter().map(|&(k, t)| (k, t.to_string())).collect();
        let ok = crate::auth::authorize_use(key, &token, &facts, kind, &target.to_string(), action);
        self.auth_cache.insert((id, kind, target, action), (fp, ok));
        ok
    }

    /// Replace (or, with empty bytes, reset) an app node's capability token.
    /// A replacement must verify against the token service's key — a token
    /// signed by a different root gates nothing and is refused. Effects follow
    /// on the next tick: the reconcilers re-filter every wire through the new
    /// token, applying what it now allows and tearing down what it doesn't.
    fn set_node_token(&mut self, id: NodeId, token: Vec<u8>) {
        if !matches!(self.graph.nodes.get(&id).map(|r| r.kind), Some(Kind::App)) {
            return;
        }
        if token.is_empty() {
            self.graph.node_tokens.remove(&id);
        } else {
            if let Some((key, _)) = &self.node_auth {
                if biscuit_auth::Biscuit::from(&token, *key).is_err() {
                    eprintln!(
                        "wk: refused token for node {id}: not signed by this \
                         workspace's token service"
                    );
                    return;
                }
            }
            self.graph.node_tokens.insert(id, token);
        }
        self.auth_cache.retain(|&(n, _, _, _), _| n != id);
        self.write_token_file(id);
    }

    /// Apply app↔app wires that were waiting on a compiling endpoint (see
    /// `pending_app_wires`): once both components have published their setup,
    /// the MIDI-vs-provider-mount classification is decidable and the wire
    /// goes through `connect_toggle` for real. A wire whose endpoint
    /// disappeared is dropped.
    fn sync_pending_app_wires(&mut self) {
        if self.pending_app_wires.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_app_wires);
        for (a, b) in pending {
            if !self.node_exists(a) || !self.node_exists(b) {
                continue;
            }
            let loading = |s: &Self, id: NodeId| s.app_node(id).is_some_and(|n| n.is_loading());
            if loading(self, a) || loading(self, b) {
                self.pending_app_wires.push((a, b));
                continue;
            }
            if !self.wired(a, b) {
                self.connect_toggle(a, b);
            }
        }
    }

    /// Reconcile the actual file mounts against the desired `connections`: mount
    /// each newly-wired volume into its app's fs at its mount path, unmount ones
    /// no longer wired. Idempotent; runs after any connection change and once per
    /// tick.
    ///
    /// Desired binds are filtered through each app's capability token first
    /// (`read` gates the mount existing at all; `write` picks read-only vs
    /// read-write), so a denied wire never mounts — and a mount whose
    /// authorization was revoked or changed mode (token swapped/attenuated) is
    /// unmounted or remounted on the next pass.
    fn sync_mounts(&mut self) {
        let binds = self.graph.connections.clone();
        let desired: Vec<(NodeId, NodeId)> = binds
            .into_iter()
            .filter(|&(f, a)| self.node_may_use(a, "file", f, "read"))
            .collect();
        let active: HashSet<(NodeId, NodeId)> = self.mounted.keys().copied().collect();
        let plan = wiring::reconcile_links(&desired, &active);
        for pair in plan.remove {
            if let Some((path, fs, _)) = self.mounted.remove(&pair) {
                crate::vfs::unmount_file(&fs, &path);
            }
        }
        for (file, app) in plan.add {
            let writable = self.node_may_use(app, "file", file, "write");
            let path = self.mount_path_for(file, app);
            let Some(node) = self.app_node(app) else {
                continue; // a node isn't resolvable yet — retried next reconcile
            };
            if let Some(f) = self.graph.file_nodes.get(&file) {
                f.mount(&node.fs, &path, writable);
            } else if let Some(provider) = self.app_node(file) {
                // The source is an app serving `wk:fs`: mount its live tree.
                // Until its component has compiled, `serves_fs` is false and
                // the mount waits (retried next reconcile).
                if !provider.serves_fs() {
                    continue;
                }
                crate::vfs::mount_provider(&node.fs, &path, provider.fs_serve.clone(), writable);
            } else {
                continue;
            }
            self.mounted
                .insert((file, app), (path, node.fs.clone(), writable));
        }
        // A live mount whose write permission flipped remounts in the new mode.
        let live: Vec<((NodeId, NodeId), bool)> = self
            .mounted
            .iter()
            .map(|(&pair, &(_, _, writable))| (pair, writable))
            .collect();
        for ((file, app), was_writable) in live {
            if self.node_may_use(app, "file", file, "write") != was_writable {
                self.remount((file, app));
            }
        }
        // Drop mount-path overrides for binds that no longer exist.
        prune_side_map(&mut self.graph.mount_paths, &self.graph.connections);
    }

    /// Re-apply the live mount for `(volume, app)` at its current effective
    /// path and write mode — used after the mount path, the volume's own
    /// source, or the app's write permission changes. A no-op if the bind
    /// isn't currently mounted.
    fn remount(&mut self, pair: (NodeId, NodeId)) {
        let Some((old, fs, _)) = self.mounted.remove(&pair) else {
            return;
        };
        crate::vfs::unmount_file(&fs, &old);
        let writable = self.node_may_use(pair.1, "file", pair.0, "write");
        let at = self.mount_path_for(pair.0, pair.1);
        if let Some(f) = self.graph.file_nodes.get(&pair.0) {
            f.mount(&fs, &at, writable);
            self.mounted.insert(pair, (at, fs, writable));
        } else if let Some(provider) = self.app_node(pair.0) {
            // A provider-app mount re-applies the same way.
            if provider.serves_fs() {
                crate::vfs::mount_provider(&fs, &at, provider.fs_serve.clone(), writable);
                self.mounted.insert(pair, (at, fs, writable));
            }
        }
    }

    /// Point a BindMount node at a host path (a file or folder), updating its
    /// default mount name from the new basename and remounting any live binds.
    fn set_bind_path(&mut self, id: NodeId, host_path: String) {
        let path = PathBuf::from(host_path.trim());
        let name = host_file_name(&path);
        match self.graph.file_nodes.get_mut(&id) {
            Some(FileNode::Bind(f)) => {
                f.path = path;
                f.name = name;
            }
            _ => return, // only BindMount nodes have a host path
        }
        let binds: Vec<(NodeId, NodeId)> = self
            .mounted
            .keys()
            .copied()
            .filter(|(vol, _)| *vol == id)
            .collect();
        for pair in binds {
            self.remount(pair);
        }
    }

    /// Set (or clear) where a bind mounts inside its app, remounting live. A path
    /// equal to the default is stored as an override too, so the choice sticks
    /// even if the volume is later renamed; an empty path resets to the default.
    fn set_mount(&mut self, volume: NodeId, app: NodeId, path: String) {
        if !self.graph.connections.contains(&(volume, app)) {
            return; // not a bind — nothing to mount
        }
        let path = path.trim();
        if path.is_empty() {
            self.graph.mount_paths.remove(&(volume, app));
        } else {
            self.graph
                .mount_paths
                .insert((volume, app), path.to_string());
        }
        self.remount((volume, app));
    }

    /// Wire (or unwire) app node `src`'s MIDI output into app node `dst`'s input.
    /// Updates the desired `midi_links` relation; routing is applied by
    /// [`Self::sync_midi`].
    fn toggle_midi(&mut self, src: NodeId, dst: NodeId) {
        wiring::toggle_pair(&mut self.graph.midi_links, src, dst);
        self.sync_midi();
    }

    /// Reconcile the MIDI router against the desired `midi_links`: add each new
    /// route (once its destination exists), drop routes no longer wired. A route
    /// needs both app endpoints' tokens to allow it (a hardware source has no
    /// token and always consents).
    fn sync_midi(&mut self) {
        let links = self.graph.midi_links.clone();
        let desired: Vec<(NodeId, NodeId)> = links
            .into_iter()
            .filter(|&(s, d)| {
                self.node_may_use(s, "midi", d, "send")
                    && self.node_may_use(d, "midi", s, "receive")
            })
            .collect();
        let plan = wiring::reconcile_links(&desired, &self.routed);
        let router = self.host.midi();
        let mut routes = router.lock().unwrap();
        for (src, dst) in plan.remove {
            routes.disconnect(src, dst);
            self.routed.remove(&(src, dst));
        }
        for (src, dst) in plan.add {
            // A MIDI link ends either at an app's input port or at a hardware
            // output node, which is a destination on the canvas the same way an
            // app is.
            let inbox = self
                .app_node(dst)
                .map(|n| n.midi_in.clone())
                .or_else(|| self.midi_out_devices.get(&dst).map(|d| d.inbox.clone()));
            if let Some(inbox) = inbox {
                routes.connect(src, dst, inbox);
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
        self.graph.pos3d.remove(&id);
        self.graph.hidden_panel3d.remove(&id);
        self.graph.node_args.remove(&id);
        self.graph.node_deps.remove(&id);
        self.graph.node_names.remove(&id);
        self.graph.file_nodes.remove(&id);
        self.graph.host_ports.remove(&id);
        self.graph.node_tokens.remove(&id);
        self.graph.iroh_secrets.remove(&id);
        self.graph.veilid_ids.remove(&id);
        self.graph.note_text.remove(&id);
        self.graph.host_services.remove(&id);
        self.graph.boundary_ports.remove(&id);
        // A boundary wire is a wire: it goes with the node it named. Left
        // behind, the group would keep drawing a line to nothing, and the file
        // it is saved into would no longer load — an `in`/`out` line whose far
        // end is not on the canvas is refused at load, on purpose.
        for g in self.graph.groups.values_mut() {
            g.in_wires.retain(|(_, n)| *n != id);
            g.out_wires.retain(|(_, n)| *n != id);
        }
        if let Some((kill, _)) = self.host_service_serves.remove(&id) {
            kill.store(true, Ordering::Relaxed);
        }
        self.auth_cache.retain(|&(n, _, _, _), _| n != id);
    }

    /// Whether the given wire still connects two live nodes.
    pub fn wire_exists(&self, w: Wire) -> bool {
        match w {
            Wire::Bind(f, a) => self.graph.connections.contains(&(f, a)),
            Wire::Midi(s, d) => self.graph.midi_links.contains(&(s, d)),
            Wire::Serve(h, hp) => self.graph.serve_links.contains(&(h, hp)),
            Wire::Capture(a, c) => self.graph.capture_links.contains(&(a, c)),
            Wire::Clipboard(a, c) => self.graph.clipboard_links.contains(&(a, c)),
            Wire::Api(a, n) => self.graph.api_links.contains(&(a, n)),
            Wire::Net(app, net) => self.graph.net_links.contains(&(app, net)),
        }
    }

    /// Remove the given connection (the same effect as toggling it off).
    fn disconnect_wire(&mut self, w: Wire) {
        match w {
            Wire::Bind(f, a) => {
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
            Wire::Capture(app, cap) => {
                if self.graph.capture_links.contains(&(app, cap)) {
                    self.toggle_capture(app, cap);
                }
            }
            Wire::Clipboard(app, clip) => {
                if self.graph.clipboard_links.contains(&(app, clip)) {
                    self.toggle_clipboard(app, clip);
                }
            }
            Wire::Api(app, api) => {
                if self.graph.api_links.contains(&(app, api)) {
                    self.toggle_api(app, api);
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
    fn set_node_pos3d(&mut self, id: NodeId, pose: [f32; 4]) {
        if self.graph.nodes.contains_key(&id) {
            self.graph.pos3d.insert(id, pose);
        }
    }
    /// Show or hide a node's flat 2D panel in the 3D world.
    fn set_node_panel3d(&mut self, id: NodeId, show: bool) {
        if self.graph.nodes.contains_key(&id) {
            if show {
                self.graph.hidden_panel3d.remove(&id);
            } else {
                self.graph.hidden_panel3d.insert(id);
            }
        }
    }

    /// One server step: reconcile any wiring that was pending on a still-loading
    /// node. Cheap; a client calls it each frame, headless in its tick loop.
    pub fn tick(&mut self) {
        // Start nodes whose run was requested while they were still compiling,
        // before reconciling serves so a just-started node gets published this
        // same tick.
        self.drain_pending_run();
        self.sync_pending_app_wires();
        self.sync_mounts();
        self.sync_midi();
        self.sync_net_membership();
        // A router's bridge follows its wires and its token, both of which can
        // change between ticks without a command ever reaching this server.
        self.sync_routers();
        self.sync_captures();
        self.sync_clipboard();
        self.sync_exec();
        self.sync_serves();
        self.sync_apis();
        self.sync_host_services();
    }

    /// Kill a node and drop everything referencing it (its wiring, geometry, and
    /// the wasm instance). Used when a client closes a node.
    fn close_node(&mut self, id: NodeId) {
        // Halt the guest (kill + wake/close its surface), then also close stdin
        // and detach the fabric — the extra teardown a permanent removal needs
        // over a restartable `stop`.
        self.halt_guest(id);
        if let Some(node) = self.app_node(id) {
            if let Some(stack) = &node.net_stack() {
                self.host.detach_net(stack);
            }
        }
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
        self.graph.clipboard_links.retain(|&(app, _)| app != id);
        self.sync_mounts();
        self.sync_midi();
        self.sync_captures();
        self.sync_clipboard();
        self.sync_serves();
        self.forget(id);
    }

    /// The sidecar directory holding persisted volume bytes, beside the `.wk`
    /// file (e.g. `workspace.wk` → `workspace.wk.volumes/`).
    fn volume_dir(&self) -> PathBuf {
        let mut s = self.workspace_path.clone().into_os_string();
        s.push(".volumes");
        PathBuf::from(s)
    }

    /// Where one persisted volume's bytes live: `<volume_dir>/<node-id>`.
    fn volume_sidecar(&self, id: NodeId) -> PathBuf {
        self.volume_dir().join(id.to_string())
    }

    /// Write every opted-in volume's bytes to its sidecar file and prune stale
    /// sidecars (volumes since made ephemeral or removed). Called by
    /// [`Self::save`] with the document it is about to write.
    ///
    /// The keep-set comes from that document, not from the live graph: a volume
    /// can be in the file without running — inside a `tab #false` definition,
    /// or on a node that failed to materialize — and it has no live bytes to
    /// re-save. Pruning by liveness would delete the user's data at shutdown
    /// for the crime of not being on screen.
    fn save_persisted_volumes(&self, doc: &Document) {
        let persisted: Vec<(NodeId, Vec<u8>)> = self
            .graph
            .file_nodes
            .iter()
            .filter_map(|(&id, f)| match f {
                FileNode::Volume(v) if v.persist => Some((id, v.data.lock().unwrap().clone())),
                _ => None,
            })
            .collect();
        let dir = self.volume_dir();
        if !persisted.is_empty() {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                eprintln!("failed to create volume dir {}: {e}", dir.display());
                return;
            }
            for (id, bytes) in &persisted {
                let path = self.volume_sidecar(*id);
                if let Err(e) = std::fs::write(&path, bytes) {
                    eprintln!("failed to persist volume {}: {e}", path.display());
                }
            }
        }
        // Remove sidecars whose volume no longer persists — every volume the
        // file still asks to persist keeps its bytes, live or not (a no-op if
        // the dir doesn't exist yet).
        let keep: HashSet<String> = doc
            .workspaces
            .iter()
            .flat_map(|w| &w.nodes)
            .filter(|n| matches!(n.kind, SnapKind::Volume { persist: true, .. }))
            .map(|n| n.id.to_string())
            .chain(persisted.iter().map(|(id, _)| id.to_string()))
            .collect();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if !keep.contains(&*entry.file_name().to_string_lossy()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    /// Each node projects through [`Self::node_snap`] — the same shape undo
    /// captures — ordered by kind then id, so saves are deterministic.
    pub fn save(&self) {
        // Every wire an expansion made. A collapsed boundary wire usually ends
        // on a derived node, but a definition that passes a connection straight
        // through joins two nodes of the parent canvas — nothing about the
        // endpoints of *that* one says who made it, so only this record keeps
        // it out of the file.
        let derived_wires: HashSet<(WireRel, NodeId, NodeId)> = self
            .instances
            .values()
            .flat_map(|rec| rec.wires.iter().copied())
            .collect();
        let mut workspaces: Vec<Workspace> = self
            .graph
            .workspaces
            .iter()
            .map(|&ws_id| {
                let mine = |id: &NodeId| self.graph.nodes.get(id).map(|n| n.ws) == Some(ws_id);
                let mut ids: Vec<NodeId> = self
                    .graph
                    .nodes
                    .iter()
                    .filter(|(_, rec)| rec.ws == ws_id)
                    .map(|(&id, _)| id)
                    .collect();
                ids.sort_by_key(|id| (self.graph.nodes[id].kind as u8, *id));
                let mut nodes: Vec<NodeSnap> =
                    ids.iter().filter_map(|&id| self.node_snap(id)).collect();
                // Re-emit any node from the loaded file that never materialized
                // (its ids are disjoint from `graph.nodes`, so no duplication).
                nodes.extend(
                    self.unplaced
                        .iter()
                        .filter(|(w, _)| *w == ws_id)
                        .map(|(_, s)| s.clone()),
                );
                // This workspace's wires of one relation: the ones whose source
                // node lives here, plus any orphan wires (touching a node that
                // never materialized) recorded against it — so both round-trip.
                //
                // A wire an expansion made is skipped on both counts. Its far
                // end is usually a derived node (`mine` is false for it, since
                // an instance is not a tab) while its near end is a real node
                // of this tab, so without the second test a collapsed boundary
                // wire would be written into the file as if the author had
                // drawn it — against an id that only exists while the server
                // runs. The third test catches the pass-through case, where
                // both ends are the tab's own nodes and the wire would outlive
                // the group that stands for it.
                let ws_wires =
                    |links: &[(NodeId, NodeId)], rel: WireRel| -> Vec<(NodeId, NodeId)> {
                        let mut v: Vec<(NodeId, NodeId)> = links
                            .iter()
                            .filter(|(a, b)| {
                                mine(a)
                                    && !self.is_derived(*b)
                                    && !derived_wires.contains(&(rel, *a, *b))
                            })
                            .copied()
                            .collect();
                        v.extend(
                            self.unplaced_wires
                                .iter()
                                .filter(|(w, r, ..)| *w == ws_id && *r == rel)
                                .map(|&(_, _, a, b)| (a, b)),
                        );
                        v
                    };
                let connections = ws_wires(&self.graph.connections, WireRel::Connection);
                let midi = ws_wires(&self.graph.midi_links, WireRel::Midi);
                let serves = ws_wires(&self.graph.serve_links, WireRel::Serve);
                let net_links = ws_wires(&self.graph.net_links, WireRel::NetLink);
                let capture_links = ws_wires(&self.graph.capture_links, WireRel::CaptureLink);
                let clipboard_links = ws_wires(&self.graph.clipboard_links, WireRel::ClipboardLink);
                let api_links = ws_wires(&self.graph.api_links, WireRel::ApiLink);
                // Persist mount-path overrides for this workspace's binds.
                let mount_paths = connections
                    .iter()
                    .filter_map(|pair| self.graph.mount_paths.get(pair).map(|p| (*pair, p.clone())))
                    .collect();
                // Persist container-port overrides for this workspace's serves.
                let serve_ports = serves
                    .iter()
                    .filter_map(|pair| self.graph.serve_ports.get(pair).map(|&p| (*pair, p)))
                    .collect();
                Workspace {
                    id: ws_id,
                    name: self.graph.workspace_names.get(&ws_id).cloned(),
                    tab: true,
                    nodes,
                    connections,
                    mount_paths,
                    midi,
                    serves,
                    serve_ports,
                    net_links,
                    capture_links,
                    clipboard_links,
                    api_links,
                }
            })
            .collect();
        // The invented tab is the loader's, not the author's: writing it back
        // would grow a stray empty `workspace` block in a definitions-only file
        // on every run. It earns its place in the file as soon as anything is
        // put in it.
        workspaces.retain(|w| Some(w.id) != self.scratch_tab || !w.nodes.is_empty());
        // Splice the authored definitions back in at the positions they were
        // loaded from. Their recorded indices are into the *whole* list, and
        // they arrive in ascending order, so by the time each one is inserted
        // the entries before it are already in place and the file's block order
        // is reproduced exactly. (`min` covers tabs closed during the session.)
        for (at, ws) in &self.authored {
            let at = (*at).min(workspaces.len());
            workspaces.insert(at, ws.clone());
        }
        // Carry the import provenance so `to_kdl` re-emits the `import` lines and
        // omits imported deps/workspaces — an autosave preserves the composition
        // rather than inlining every imported file.
        let doc = Document {
            imports: self.imports.clone(),
            dependencies: self.graph.available.clone(),
            workspaces,
            imported_deps: self.imported_deps.clone(),
            imported_workspaces: self.imported_workspaces.clone(),
            scratch_tab: self.scratch_tab,
        };
        self.save_persisted_volumes(&doc);
        if let Err(e) = doc.save(&self.workspace_path) {
            eprintln!("failed to save workspace: {e}");
        }
    }

    /// Apply a client [`Command`], recording an inverse for [`Command::Undo`]
    /// where the mutation is undoable. The single entry point for mutations.
    pub fn apply(&mut self, cmd: Command) {
        if let Some(why) = self.refuse_structural_edit(&cmd) {
            eprintln!("wk: {why}");
            return;
        }
        match &cmd {
            // Node creates: run, then record removal of whatever node appeared.
            Command::Create(Resource::Node { .. } | Resource::HostMount { .. })
            | Command::Duplicate(_) => {
                let before: HashSet<NodeId> = self.graph.nodes.keys().copied().collect();
                self.dispatch(cmd);
                let created: Vec<NodeId> = self
                    .graph
                    .nodes
                    .keys()
                    .copied()
                    .filter(|id| !before.contains(id))
                    .collect();
                if !created.is_empty() {
                    self.record(Undo::Uncreate(created));
                }
                return;
            }
            Command::Create(Resource::Wire { a, b }) => {
                // Only record when the create will actually connect.
                if !self.wired(*a, *b) {
                    // Net/serve wires are "one per source": connecting may
                    // displace an existing link, which undo must restore.
                    match wiring::classify(*a, *b, self.class_of(*a), self.class_of(*b)) {
                        Some(Wire::Net(app, net)) => {
                            let old_dst = self
                                .graph
                                .net_links
                                .iter()
                                .find(|&&(s, _)| s == app)
                                .map(|&(_, d)| d);
                            self.record(Undo::RewireUnique {
                                src: app,
                                new_dst: net,
                                old_dst,
                            });
                        }
                        Some(Wire::Serve(http, hostport)) => {
                            let old_dst = self
                                .graph
                                .serve_links
                                .iter()
                                .find(|&&(s, _)| s == http)
                                .map(|&(_, d)| d);
                            self.record(Undo::RewireUnique {
                                src: http,
                                new_dst: hostport,
                                old_dst,
                            });
                        }
                        _ => self.record(Undo::Wire(*a, *b)),
                    }
                }
            }
            Command::Create(Resource::Workspace { id }) => {
                if !self.graph.workspaces.contains(id) {
                    self.record(Undo::DropWorkspace(*id));
                }
            }
            // Authoring or removing a boundary wire is one edit and one undo
            // entry, however many live wires the re-expansion moves: the line
            // in the file is the thing that changed. Recorded *after* the fact
            // — the expansion may refuse the line, and a refusal that still
            // cost a press of Ctrl-Z would undo the edit before it instead.
            Command::Create(Resource::Boundary(bw))
            | Command::Delete(ResourceRef::Boundary(bw)) => {
                let line = bw.clone();
                let before = self.boundary_wired(&line);
                self.dispatch(cmd);
                if self.boundary_wired(&line) != before {
                    self.record(Undo::Boundary(line, before));
                }
                return;
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
                if patch.text.is_some() {
                    if let Some(t) = self.graph.note_text.get(id).cloned() {
                        self.record(Undo::Text(*id, t));
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
            Command::SetToken { id, .. } => {
                if matches!(self.graph.nodes.get(id).map(|r| r.kind), Some(Kind::App)) {
                    self.record(Undo::Token(*id, self.graph.node_tokens.get(id).cloned()));
                }
            }
            // Not undoable: run, mount-path / serve-port edits, and undo itself.
            Command::SetMount { .. }
            | Command::SetServePort { .. }
            | Command::Run(_)
            | Command::Stop(_)
            | Command::SetView(_)
            | Command::Undo => {}
        }
        self.dispatch(cmd);
    }

    /// Why this command must be refused, if its target is inside an instance.
    ///
    /// An instance's nodes are *derived*: they exist because a definition says
    /// so, and nothing about them can be written back to the `.wk` file. A
    /// structural edit there would therefore be undone by the next restart —
    /// silently, and after the user had already built on it. v1 answers "no"
    /// out loud instead; editing the definition is what changes every instance
    /// of it. Everything that is not structure (running a node, moving it,
    /// changing its args) is left alone.
    fn refuse_structural_edit(&self, cmd: &Command) -> Option<String> {
        let inside = |id: NodeId| -> Option<String> {
            self.is_derived(id).then(|| {
                let label = self
                    .graph
                    .nodes
                    .get(&id)
                    .map(|rec| self.instance_label(rec.ws))
                    .unwrap_or_default();
                format!("{id} is inside instance {label:?}; edit the definition instead")
            })
        };
        match cmd {
            Command::Delete(ResourceRef::Node(id)) | Command::Duplicate(id) => inside(*id),
            Command::Create(Resource::Wire { a, b }) => inside(*a).or_else(|| inside(*b)),
            // A boundary wire on a group that is itself inside an instance is
            // the *definition's* line, not this canvas's: it would apply live
            // and be gone on the next load, since nothing derived is written
            // back. The far end is checked too — it is a node like any other.
            Command::Create(Resource::Boundary(bw))
            | Command::Delete(ResourceRef::Boundary(bw)) => {
                inside(bw.group).or_else(|| inside(bw.node))
            }
            Command::Delete(ResourceRef::Wire(w)) => {
                let (a, b) = wire_ends(*w);
                inside(a).or_else(|| inside(b))
            }
            // A node added to an instance's canvas would vanish on restart for
            // the same reason, and there is nowhere to write it down.
            Command::Create(Resource::Node { ws, .. } | Resource::HostMount { ws, .. }) => self
                .instances
                .contains_key(ws)
                .then(|| format!("{ws} is an instance; add the node to its definition instead")),
            _ => None,
        }
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
                NodeKind::Volume => self.add_virtual_file(pos, ws),
                NodeKind::BindMount => self.add_host_mapped_file(pos, ws),
                NodeKind::Port => self.add_host_port(pos, ws),
                NodeKind::Network => {
                    self.add_net_node(pos, ws);
                }
                NodeKind::Gateway => self.add_gateway_node(pos, ws),
                NodeKind::Router => self.add_router_node(pos, ws),
                NodeKind::Iroh => self.add_iroh_node(pos, ws),
                NodeKind::Veilid => self.add_veilid_node(pos, ws),
                NodeKind::Note => self.add_note(pos, ws),
                NodeKind::Capture => self.add_capture_node(pos, ws),
                NodeKind::Clipboard => self.add_clipboard_node(pos, ws),
                NodeKind::Api => self.add_api_node(pos, ws),
                NodeKind::MidiIn => self.add_midi_in_node(pos, ws),
                NodeKind::MidiOut => self.add_midi_out_node(pos, ws),
                NodeKind::HostService => self.add_host_service(pos, ws),
            },
            // Create is create only: a wire that already exists is left alone
            // (removal is Delete, so a create-only token can never disconnect).
            Command::Create(Resource::HostMount { path, pos, ws }) => {
                self.add_host_mount(PathBuf::from(path), pos, ws)
            }
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
                if let Some(pose) = patch.pos3d {
                    self.set_node_pos3d(id, pose);
                }
                if let Some(show) = patch.panel3d {
                    self.set_node_panel3d(id, show);
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
                if let Some(port) = patch.port_set {
                    self.set_host_port(id, port);
                }
                if let Some(text) = patch.text {
                    if self.graph.note_text.contains_key(&id) {
                        self.graph.note_text.insert(id, text);
                    }
                }
                if let Some(host_path) = patch.host_path {
                    self.set_bind_path(id, host_path);
                }
                if let Some(device) = patch.midi_device {
                    self.set_midi_device(id, device);
                }
                if let Some(name) = patch.service_name {
                    if let Some(svc) = self.graph.host_services.get_mut(&id) {
                        let name = name.trim();
                        if !name.is_empty() {
                            svc.name = name.to_string();
                        }
                    }
                }
                if let Some(target) = patch.service_target {
                    if let Some(svc) = self.graph.host_services.get_mut(&id) {
                        svc.target = target.trim().to_string();
                    }
                }
                if let Some(persist) = patch.persist {
                    if let Some(FileNode::Volume(v)) = self.graph.file_nodes.get_mut(&id) {
                        v.persist = persist;
                    }
                }
            }
            Command::Create(Resource::Boundary(bw)) => self.set_boundary_wire(&bw, true),
            Command::Delete(ResourceRef::Boundary(bw)) => self.set_boundary_wire(&bw, false),
            Command::Delete(ResourceRef::Node(id)) => self.remove_any(id),
            Command::Delete(ResourceRef::Wire(w)) => self.disconnect_wire(w),
            Command::Delete(ResourceRef::Workspace(id)) => self.remove_workspace(id),
            Command::SetMount { volume, app, path } => self.set_mount(volume, app, path),
            Command::SetServePort {
                served,
                hostport,
                container,
            } => self.set_serve_port(served, hostport, container),
            Command::SetToken { id, token } => self.set_node_token(id, token),
            Command::Run(id) => self.run_node(id),
            Command::Stop(id) => self.stop_node(id),
            Command::Duplicate(id) => self.duplicate(id),
            Command::SetView(mode) => self.view_mode = (self.view_mode.0 + 1, mode),
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
            (Some(Undo::Text(a, _)), Undo::Text(b, _)) => a == b,
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
                // Route through set_node_args so an uplink re-dials (or undials)
                // the restored ticket instead of the live dialer diverging from
                // the persisted args.
                if self.graph.nodes.contains_key(&id) {
                    self.set_node_args(id, &a.join(" "));
                }
            }
            Undo::Text(id, t) => {
                if self.graph.note_text.contains_key(&id) {
                    self.graph.note_text.insert(id, t);
                }
            }
            Undo::Port(id, port) => {
                if let Some(&cur) = self.graph.host_ports.get(&id) {
                    self.change_port(id, port as i32 - cur as i32);
                }
            }
            Undo::RewireUnique {
                src,
                new_dst,
                old_dst,
            } => {
                // Drop the new wire (toggle it off), then restore the displaced
                // one. connect_toggle reclassifies by kind, so orientation is
                // handled the same as the forward connect.
                if self.node_exists(src) && self.node_exists(new_dst) && self.wired(src, new_dst) {
                    self.connect_toggle(src, new_dst);
                }
                if let Some(old) = old_dst {
                    if self.node_exists(src) && self.node_exists(old) && !self.wired(src, old) {
                        self.connect_toggle(src, old);
                    }
                }
            }
            Undo::Wire(a, b) => {
                if self.node_exists(a) && self.node_exists(b) {
                    self.connect_toggle(a, b);
                }
            }
            Undo::Token(id, old) => {
                if self.node_exists(id) {
                    // Restore directly (set_node_token would re-verify — the old
                    // value was ours, and it may legitimately be "no custom token").
                    match old {
                        Some(t) => self.graph.node_tokens.insert(id, t),
                        None => self.graph.node_tokens.remove(&id),
                    };
                    self.auth_cache.retain(|&(n, _, _, _), _| n != id);
                    self.write_token_file(id);
                }
            }
            Undo::Uncreate(ids) => {
                // Guarded per id: removing a `group` already took its instance
                // with it, so the derived nodes in the same entry are gone.
                for id in ids {
                    if self.node_exists(id) {
                        self.remove_any(id);
                    }
                }
            }
            Undo::Boundary(bw, wired) => self.set_boundary_wire(&bw, wired),
            Undo::Recreate(s) => self.recreate(*s),
            Undo::DropWorkspace(id) => self.remove_workspace(id),
            Undo::RecreateWorkspace(s) => self.recreate_workspace(*s),
        }
    }

    /// Project node `id` into the persisted shape ([`NodeSnap`]) — the same
    /// projection [`Self::save`] writes to the `.wk` file.
    fn node_snap(&self, id: NodeId) -> Option<NodeSnap> {
        let &NodeRec {
            ws: _,
            pos,
            size,
            kind,
        } = self.graph.nodes.get(&id)?;
        let kind = match kind {
            Kind::App => {
                let node = self.app_node(id)?;
                let options = node.options.lock().unwrap().clone();
                SnapKind::App {
                    dep: self
                        .graph
                        .node_deps
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| node.name.clone()),
                    // Written only when it differs from the type: a lone
                    // `python` node stays a plain `node "python"` line.
                    // Written only when someone chose it. A generated name is
                    // a function of the id, and the id is already on this very
                    // line, so writing it down would add a line that says
                    // nothing — and a *walked* name is not that function, so
                    // it is written and stops being able to drift.
                    name: self
                        .graph
                        .node_names
                        .get(&id)
                        .filter(|name| **name != crate::nodename::generated(id, 0))
                        .cloned(),
                    options,
                    args: self.graph.node_args.get(&id).cloned().unwrap_or_default(),
                    token: self
                        .graph
                        .node_tokens
                        .get(&id)
                        .map(|t| crate::workspace::bytes_hex(t)),
                }
            }
            Kind::File => match self.graph.file_nodes.get(&id)? {
                FileNode::Volume(v) => SnapKind::Volume {
                    name: v.name.clone(),
                    persist: v.persist,
                },
                FileNode::Bind(h) => SnapKind::BindMount {
                    path: h.path.clone(),
                },
            },
            Kind::Port => SnapKind::Port {
                port: *self.graph.host_ports.get(&id)?,
            },
            Kind::Network => SnapKind::Net { gateway: false },
            Kind::Gateway => SnapKind::Net { gateway: true },
            Kind::Router => SnapKind::Router,
            Kind::Iroh => SnapKind::Iroh {
                secret: self.graph.iroh_secrets.get(&id).map(secret_hex),
                peer: self.peer_ticket(id),
            },
            Kind::Veilid => SnapKind::Veilid {
                secret: self.graph.veilid_ids.get(&id).cloned(),
                peer: self.peer_ticket(id),
            },
            Kind::Note => SnapKind::Note {
                text: self.graph.note_text.get(&id).cloned().unwrap_or_default(),
            },
            Kind::Capture => SnapKind::Capture,
            Kind::Clipboard => SnapKind::Clipboard,
            Kind::Api => SnapKind::Api,
            Kind::MidiIn => SnapKind::MidiIn {
                device: self.graph.midi_ins.get(&id).cloned().unwrap_or_default(),
            },
            Kind::MidiOut => SnapKind::MidiOut {
                device: self.graph.midi_outs.get(&id).cloned().unwrap_or_default(),
            },
            Kind::HostService => {
                let svc = self.graph.host_services.get(&id)?;
                SnapKind::HostService {
                    name: svc.name.clone(),
                    target: svc.target.clone(),
                }
            }
            Kind::Boundary => {
                let p = self.graph.boundary_ports.get(&id)?;
                match p.dir {
                    PortDir::In => SnapKind::InPort {
                        name: p.name.clone(),
                        kind: p.kind,
                    },
                    PortDir::Out => SnapKind::OutPort {
                        name: p.name.clone(),
                        kind: p.kind,
                    },
                }
            }
            Kind::Group => {
                let g = self.graph.groups.get(&id)?;
                SnapKind::Group {
                    definition: g.definition.clone(),
                    name: g.name.clone(),
                    in_wires: g.in_wires.clone(),
                    out_wires: g.out_wires.clone(),
                }
            }
        };
        Some(NodeSnap {
            id,
            pos,
            size,
            pos3d: self.graph.pos3d.get(&id).copied(),
            panel3d: !self.graph.hidden_panel3d.contains(&id),
            kind,
        })
    }

    /// An uplink node's dialed peer ticket (rides its args), if set.
    fn peer_ticket(&self, id: NodeId) -> Option<String> {
        self.graph
            .node_args
            .get(&id)
            .filter(|a| !a.is_empty())
            .map(|a| a.join(" "))
    }

    /// Capture everything needed to bring node `id` back after removal.
    fn snapshot(&self, id: NodeId) -> Option<Snapshot> {
        let ws = self.graph.nodes.get(&id)?.ws;
        let node = self.node_snap(id)?;
        // A Volume's bytes are runtime-only state: carried for undo,
        // never persisted.
        let file_data = match self.graph.file_nodes.get(&id) {
            Some(FileNode::Volume(v)) => v.data.lock().unwrap().clone(),
            _ => Vec::new(),
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
        wires.extend(
            self.graph
                .capture_links
                .iter()
                .filter(|&&(a, c)| a == id || c == id),
        );
        wires.extend(
            self.graph
                .clipboard_links
                .iter()
                .filter(|&&(a, c)| a == id || c == id),
        );
        wires.extend(
            self.graph
                .api_links
                .iter()
                .filter(|&&(a, n)| a == id || n == id),
        );
        let mut boundary: Vec<BoundaryWire> = Vec::new();
        for (&group, g) in &self.graph.groups {
            for (dir, wires) in [(PortDir::In, &g.in_wires), (PortDir::Out, &g.out_wires)] {
                for (port, node) in wires.iter().filter(|(_, n)| *n == id) {
                    boundary.push(BoundaryWire {
                        group,
                        dir,
                        port: port.clone(),
                        node: *node,
                    });
                }
            }
        }
        Some(Snapshot {
            ws,
            node,
            file_data,
            wires,
            boundary,
        })
    }

    /// Bring a removed node back with the same id, then re-establish its wiring.
    /// A `group` brings its whole instance back with it — the ids are derived
    /// from the file, so what comes back is what was there.
    fn recreate(&mut self, s: Snapshot) {
        self.materialize(s.ws, &s.node, &s.file_data);
        self.rewire(&s.wires);
        // The instances that were wired to it are still live, so re-authoring
        // their lines re-expands them onto the node that just came back.
        for bw in &s.boundary {
            self.set_boundary_wire(bw, true);
        }
        if matches!(s.node.kind, SnapKind::Group { .. }) {
            self.expand_group(s.ws, s.node.id);
        }
    }

    /// Materialize a node from its persisted shape into workspace `ws` — the
    /// single creation path shared by load-time restore and undo. `file_data`
    /// seeds a Volume's bytes (undo has them; the `.wk` file doesn't).
    fn materialize(&mut self, ws: NodeId, s: &NodeSnap, file_data: &[u8]) {
        match &s.kind {
            SnapKind::App {
                dep: dep_name,
                name,
                options,
                args,
                token,
            } => {
                let Some(dep) = self
                    .graph
                    .available
                    .iter()
                    .find(|d| &d.name == dep_name)
                    .cloned()
                else {
                    eprintln!("workspace references unknown dependency {dep_name:?}; skipping");
                    return;
                };
                // Unnamed means "called after your type", which is what a
                // workspace holding one of each wants. A second node of a type
                // is disambiguated here, in file order, so the first one in the
                // file keeps the plain name however many arrive later.
                let ident = match name {
                    Some(n) => n.clone(),
                    None => self.generated_node_name(s.id),
                };
                // The node's saved (possibly-edited) args, else the dependency
                // default (the file format doesn't distinguish "no args saved"
                // from "explicitly none").
                let args = if args.is_empty() {
                    dep.effective_args()
                } else {
                    args.clone()
                };
                self.graph.node_deps.insert(s.id, dep.name.clone());
                self.graph.node_names.insert(s.id, ident.clone());
                if let Err(e) = self.host.spawn(
                    &dep.local_path(),
                    &ident,
                    s.id,
                    &args,
                    self.registry.clone(),
                    self.node_reg.clone(),
                    options.clone(),
                    dep.container(),
                ) {
                    eprintln!("failed to restore {}: {e:#}", dep.name);
                    return;
                }
                self.place(s.id, Kind::App, ws, s.pos, s.size);
                self.graph.node_args.insert(s.id, args);
                // Restore a custom capability token. One that doesn't verify
                // (the key file was lost, or the .wk moved to another machine)
                // is dropped — the node falls back to the default token.
                if let Some(tok) = token
                    .as_deref()
                    .and_then(crate::workspace::hex_bytes)
                    .filter(|t| !t.is_empty())
                {
                    match &self.node_auth {
                        Some((key, _)) if biscuit_auth::Biscuit::from(&tok, *key).is_err() => {
                            eprintln!(
                                "wk: node {} has a token from a different token service; \
                                 using the default",
                                s.id
                            );
                        }
                        _ => {
                            self.graph.node_tokens.insert(s.id, tok);
                        }
                    }
                }
                self.write_token_file(s.id);
            }
            SnapKind::Volume { name, persist } => {
                if let Some(num) = name
                    .strip_prefix("file")
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    self.file_seq = self.file_seq.max(num);
                }
                // A persisted volume with no undo bytes seeds from its sidecar.
                let data = if *persist && file_data.is_empty() {
                    std::fs::read(self.volume_sidecar(s.id)).unwrap_or_default()
                } else {
                    file_data.to_vec()
                };
                self.place(s.id, Kind::File, ws, s.pos, s.size);
                self.graph.file_nodes.insert(
                    s.id,
                    FileNode::Volume(Volume {
                        name: name.clone(),
                        data: Arc::new(Mutex::new(data)),
                        persist: *persist,
                    }),
                );
            }
            SnapKind::BindMount { path } => {
                let name = host_file_name(path);
                if let Some(num) = name
                    .strip_prefix("host")
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    self.host_seq = self.host_seq.max(num);
                }
                self.place(s.id, Kind::File, ws, s.pos, s.size);
                self.graph.file_nodes.insert(
                    s.id,
                    FileNode::Bind(BindMount {
                        name,
                        path: path.clone(),
                    }),
                );
            }
            SnapKind::Port { port } => {
                self.next_port = self.next_port.max(port.saturating_add(1));
                self.place(s.id, Kind::Port, ws, s.pos, s.size);
                self.graph.host_ports.insert(s.id, *port);
            }
            SnapKind::Net { gateway } => {
                let kind = if *gateway {
                    Kind::Gateway
                } else {
                    Kind::Network
                };
                self.place(s.id, kind, ws, s.pos, s.size);
            }
            SnapKind::Router => {
                self.place(s.id, Kind::Router, ws, s.pos, s.size);
                self.sync_routers();
            }
            SnapKind::Iroh { secret, peer } => {
                let secret = secret.as_deref().and_then(secret_bytes);
                self.create_uplink(s.id, secret, s.pos, s.size, ws);
                if let Some(peer) = peer {
                    self.set_node_args(s.id, peer);
                }
            }
            SnapKind::Veilid { secret, peer } => {
                self.create_veilid_uplink(s.id, secret.as_deref(), s.pos, s.size, ws);
                if let Some(peer) = peer {
                    self.set_node_args(s.id, peer);
                }
            }
            SnapKind::Note { text } => {
                self.place(s.id, Kind::Note, ws, s.pos, s.size);
                self.graph.note_text.insert(s.id, text.clone());
            }
            SnapKind::Capture => {
                self.place(s.id, Kind::Capture, ws, s.pos, s.size);
                self.capture_feeds
                    .entry(s.id)
                    .or_insert_with(crate::capture::new_slot);
            }
            SnapKind::Clipboard => {
                self.place(s.id, Kind::Clipboard, ws, s.pos, s.size);
                self.clipboard_boards
                    .entry(s.id)
                    .or_insert_with(crate::clipboard::new_board);
            }
            SnapKind::Api => {
                self.place(s.id, Kind::Api, ws, s.pos, s.size);
            }
            SnapKind::MidiIn { device } => {
                self.place(s.id, Kind::MidiIn, ws, s.pos, s.size);
                self.graph.midi_ins.insert(s.id, device.clone());
                // Reconnect to the saved device (or the first available).
                self.open_midi_device(s.id, device);
            }
            SnapKind::MidiOut { device } => {
                self.place(s.id, Kind::MidiOut, ws, s.pos, s.size);
                self.graph.midi_outs.insert(s.id, device.clone());
                self.open_midi_out_device(s.id, device);
            }
            SnapKind::HostService { name, target } => {
                self.place(s.id, Kind::HostService, ws, s.pos, s.size);
                self.graph.host_services.insert(
                    s.id,
                    HostService {
                        name: name.clone(),
                        target: target.clone(),
                    },
                );
            }
            // A boundary port is placed and wired like any other node, but it
            // spawns nothing and reconciles nothing: in a plain tab there is no
            // other side of the boundary for its wires to reach.
            SnapKind::InPort { name, kind } | SnapKind::OutPort { name, kind } => {
                let (dir, _) = s.kind.boundary().expect("an in/out port is a boundary");
                self.place(s.id, Kind::Boundary, ws, s.pos, s.size);
                self.graph.boundary_ports.insert(
                    s.id,
                    BoundaryPort {
                        name: name.clone(),
                        dir,
                        kind: *kind,
                    },
                );
            }
            // A `group` places the instance's *handle* — the node the canvas
            // draws and the file writes back. What it stands for is materialized
            // by `expand_group`, once every node its boundary wires reach is on
            // the canvas.
            SnapKind::Group {
                definition,
                name,
                in_wires,
                out_wires,
            } => {
                self.place(s.id, Kind::Group, ws, s.pos, s.size);
                self.graph.groups.insert(
                    s.id,
                    GroupNode {
                        definition: definition.clone(),
                        name: name.clone(),
                        in_wires: in_wires.clone(),
                        out_wires: out_wires.clone(),
                    },
                );
            }
        }
        if let Some(p3) = s.pos3d {
            self.graph.pos3d.insert(s.id, p3);
        }
        if !s.panel3d {
            self.graph.hidden_panel3d.insert(s.id);
        }
    }

    /// Whether two nodes are already joined by any connection.
    fn wired(&self, a: NodeId, b: NodeId) -> bool {
        let pair = |x: NodeId, y: NodeId| (x == a && y == b) || (x == b && y == a);
        self.graph.connections.iter().any(|&(x, y)| pair(x, y))
            || self.graph.midi_links.iter().any(|&(x, y)| pair(x, y))
            || self.graph.net_links.iter().any(|&(x, y)| pair(x, y))
            || self.graph.serve_links.iter().any(|&(h, hp)| pair(h, hp))
            || self.graph.capture_links.iter().any(|&(x, y)| pair(x, y))
            || self.graph.clipboard_links.iter().any(|&(x, y)| pair(x, y))
            || self.graph.api_links.iter().any(|&(x, y)| pair(x, y))
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
            name: self.graph.workspace_names.get(&ws).cloned(),
            nodes,
        })
    }

    /// Bring a removed workspace back: its tab, all its nodes, then their wiring.
    ///
    /// Only a *tab* is ever recreated this way. A snapshot holds the nodes whose
    /// `ws` is the tab, so an instance's derived nodes are not among them — the
    /// `group` node is, and expanding it again is what brings them back.
    fn recreate_workspace(&mut self, s: WsSnapshot) {
        if !self.graph.workspaces.contains(&s.id) {
            let i = s.index.min(self.graph.workspaces.len());
            self.graph.workspaces.insert(i, s.id);
        }
        if let Some(name) = s.name {
            self.graph.workspace_names.insert(s.id, name);
        }
        for node in &s.nodes {
            self.materialize(node.ws, &node.node, &node.file_data);
        }
        for node in &s.nodes {
            self.rewire(&node.wires);
        }
        // Last, once every node a boundary wire could reach is back.
        for node in &s.nodes {
            if matches!(node.node.kind, SnapKind::Group { .. }) {
                self.expand_group(node.ws, node.node.id);
            }
        }
    }

    /// Every `group` node as the client sees it. A group's edges are the
    /// definition's boundary ports, which live in the authored workspace — the
    /// live graph never holds them, since expansion collapses each one into the
    /// wire that crosses it.
    fn group_infos(&self) -> HashMap<NodeId, GroupInfo> {
        self.graph
            .groups
            .iter()
            .map(|(&id, g)| {
                let ports = self
                    .authored
                    .iter()
                    .find(|(_, w)| w.name.as_deref() == Some(&g.definition))
                    .map(|(_, w)| {
                        w.nodes
                            .iter()
                            .filter_map(|n| {
                                let (dir, kind) = n.kind.boundary()?;
                                let name = match &n.kind {
                                    SnapKind::InPort { name, .. }
                                    | SnapKind::OutPort { name, .. } => name.clone(),
                                    _ => return None,
                                };
                                Some(BoundaryPort { name, dir, kind })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let nodes = self.instance_size(id);
                (
                    id,
                    GroupInfo {
                        definition: g.definition.clone(),
                        ports,
                        in_wires: g.in_wires.clone(),
                        out_wires: g.out_wires.clone(),
                        nodes,
                    },
                )
            })
            .collect()
    }

    /// How many live nodes an instance holds, counting every instance nested
    /// inside it — what an instance's widget reports as its size.
    fn instance_size(&self, id: NodeId) -> usize {
        let Some(rec) = self.instances.get(&id) else {
            return 0;
        };
        rec.nodes.len()
            + self
                .instances
                .iter()
                .filter(|(_, r)| r.parent == Some(id))
                .map(|(&child, _)| self.instance_size(child))
                .sum::<usize>()
    }

    /// A read-only snapshot of everything a client needs to render this frame.
    /// Taken under a single lock by the runtime and handed to clients so none of
    /// them holds a live lock on the server (and so the shape is exactly what a
    /// networked client would receive over the wire).
    pub fn view(&mut self) -> View {
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
                        host_mapped: matches!(f, FileNode::Bind(_)),
                        is_dir: matches!(f, FileNode::Bind(h) if h.path.is_dir()),
                        persist: matches!(f, FileNode::Volume(v) if v.persist),
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
        let routers = self
            .graph
            .nodes
            .iter()
            .filter(|(_, r)| r.kind == Kind::Router)
            .map(|(&id, _)| id)
            .collect();
        let gateways = self
            .graph
            .nodes
            .iter()
            .filter(|(_, r)| r.kind == Kind::Gateway)
            .map(|(&id, _)| id)
            .collect();
        let api_nodes = self
            .graph
            .nodes
            .iter()
            .filter(|(_, r)| r.kind == Kind::Api)
            .map(|(&id, _)| id)
            .collect();
        // Provider capability comes from the compiled component (like the typed
        // MIDI/Net ports), not the graph — empty until a compile publishes it.
        let fs_providers = nodes
            .iter()
            .filter(|n| n.serves_fs())
            .map(|n| n.id)
            .collect();
        let uplinks = self
            .uplinks
            .iter()
            .map(|(&id, up)| {
                (
                    id,
                    UplinkMeta {
                        kind: up.kind(),
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
            pos3d: self.graph.pos3d.clone(),
            hidden_panel3d: self.graph.hidden_panel3d.clone(),
            // A node's wk:scene objects render only while its token allows
            // "show" — the deny side is a live per-viewer mute: the guest keeps
            // its entity (and keeps updating it); it just isn't in the view.
            scene_entities: {
                let entities = self.host.scene_entities();
                entities
                    .into_iter()
                    .filter(|e| {
                        let owner = e.lock().unwrap().node_id;
                        self.node_may_use(owner, "scene", owner, "show")
                    })
                    .collect()
            },
            view_mode: self.view_mode,
            file_nodes,
            host_ports: self.graph.host_ports.clone(),
            notes: self.graph.note_text.clone(),
            midi_ins: self.graph.midi_ins.clone(),
            midi_outs: self.graph.midi_outs.clone(),
            host_services: self.graph.host_services.clone(),
            boundary_ports: self.graph.boundary_ports.clone(),
            groups: self.group_infos(),
            net_nodes,
            routers,
            node_labels: self
                .graph
                .node_deps
                .iter()
                .map(|(&id, dep)| {
                    let chosen = self
                        .graph
                        .node_names
                        .get(&id)
                        .filter(|n| **n != crate::nodename::generated(id, 0));
                    (id, chosen.cloned().unwrap_or_else(|| dep.clone()))
                })
                .collect(),
            gateways,
            uplinks,
            connections: self.graph.connections.clone(),
            mount_paths: self.graph.mount_paths.clone(),
            fs_providers,
            midi_links: self.graph.midi_links.clone(),
            net_links: self.graph.net_links.clone(),
            capture_links: self.graph.capture_links.clone(),
            clipboard_links: self.graph.clipboard_links.clone(),
            api_links: self.graph.api_links.clone(),
            serve_ports: self.graph.serve_ports.clone(),
            capture_feeds: self.capture_feeds.clone(),
            clipboard_boards: self.clipboard_boards.clone(),
            api_nodes,
            attached: self.attached.clone(),
            serves,
            port_errors: self.port_errors.clone(),
            node_args: self.graph.node_args.clone(),
            available: self.graph.available.clone(),
            nodes,
            surfaces,
            node_ws,
            workspaces: self.graph.workspaces.clone(),
            workspace_names: self.graph.workspace_names.clone(),
        }
    }

    /// Mark (or clear) a node as externally attached by a CLI client. Returns
    /// whether it is a terminal node the client can actually stream (so an
    /// attach to a non-terminal node is rejected without leaving it flagged).
    pub fn set_attached(&mut self, id: NodeId, on: bool) -> bool {
        let is_terminal = self.app_node(id).is_some_and(|n| n.is_command());
        if on {
            if is_terminal {
                self.attached.insert(id);
            }
        } else {
            self.attached.remove(&id);
        }
        is_terminal
    }

    /// A serializable projection of the state for a remote (CLI) client — the
    /// wire form of [`Self::view`], carrying only plain data (no shared runtime
    /// handles). See [`wk_protocol::ipc::Snapshot`].
    pub fn ipc_snapshot(&mut self) -> wk_protocol::ipc::Snapshot {
        use wk_protocol::ipc::{NodeInfo, Snapshot, WireInfo};
        let v = self.view();
        // Every node's fabric addresses, collected once: locking each stack
        // from inside the per-node closure below would re-lock per field.
        let fabric_addrs: HashMap<NodeId, (String, String)> = v
            .nodes
            .iter()
            .filter_map(|n| {
                let stack = n.net_stack()?;
                let g = stack.lock().unwrap();
                Some((n.id, (g.ip.to_string(), g.ip6.to_string())))
            })
            .collect();
        let kind_str = |id: NodeId| -> &'static str {
            match self.kind_of(id) {
                Some(Kind::App) => "app",
                Some(Kind::File) => {
                    if v.file_nodes.get(&id).is_some_and(|f| f.host_mapped) {
                        "bindmount"
                    } else {
                        "volume"
                    }
                }
                Some(Kind::Port) => "hostport",
                Some(Kind::Network) => "network",
                Some(Kind::Gateway) => "gateway",
                Some(Kind::Router) => "router",
                Some(Kind::Iroh) => "iroh",
                Some(Kind::Veilid) => "veilid",
                Some(Kind::Note) => "note",
                Some(Kind::Capture) => "capture",
                Some(Kind::Clipboard) => "clipboard",
                Some(Kind::Api) => "api",
                Some(Kind::MidiIn) => "midiin",
                Some(Kind::MidiOut) => "midiout",
                Some(Kind::HostService) => "hostservice",
                Some(Kind::Boundary) => match self.graph.boundary_ports.get(&id).map(|p| p.dir) {
                    Some(PortDir::Out) => "outport",
                    _ => "inport",
                },
                Some(Kind::Group) => "group",
                None => "unknown",
            }
        };
        // Where a node sits, as the CLI should print it: an instance is not a
        // tab, so a derived node reports the tab its instance is shown in and
        // carries the instance in its *name* instead — which is also what makes
        // two instances of one definition tellable apart.
        let placement = |id: NodeId| -> (NodeId, String) {
            let ws = v.node_ws.get(&id).copied().unwrap_or_default();
            match self.instances.get(&ws) {
                Some(rec) => (rec.tab, format!("{}/", rec.name)),
                None => (ws, String::new()),
            }
        };
        let nodes = v
            .node_ids
            .iter()
            .map(|&id| {
                let app = v.app_node(id);
                let name = if let Some(n) = &app {
                    n.name.clone()
                } else if let Some(f) = v.file_nodes.get(&id) {
                    f.name.clone()
                } else if let Some(p) = v.boundary_ports.get(&id) {
                    // A boundary port's name is how a wire from outside picks
                    // it, so it is the one thing `wk ps` must show.
                    p.name.clone()
                } else if v.groups.contains_key(&id) {
                    // An instance's own name is the scope every node inside it
                    // is called after, so `voice-2` and `voice-2-arp` read as
                    // one thing without any prefixing.
                    self.instance_label(id)
                } else {
                    String::new()
                };
                let (ws, scope) = placement(id);
                // An app node's name is its own and unique wherever it lives,
                // so it is printed bare — it is what `wk logs` takes and what a
                // peer dials. A volume or a port has no identity of its own,
                // only a role inside its instance, so that one is qualified or
                // two instances' `chan` volumes read identically.
                let scope = if v.app_node(id).is_some() || v.groups.contains_key(&id) {
                    String::new()
                } else {
                    scope
                };
                NodeInfo {
                    id,
                    kind: kind_str(id).to_string(),
                    name: format!("{scope}{name}"),
                    // An instance's type is the definition it stamps out, the
                    // same way an app node's is the dependency it runs.
                    node_type: self
                        .graph
                        .node_deps
                        .get(&id)
                        .or_else(|| self.instances.get(&id).map(|rec| &rec.definition))
                        .cloned()
                        .unwrap_or_default(),
                    ws,
                    pos: v.win_pos.get(&id).copied().unwrap_or([0.0, 0.0]),
                    size: v.win_size.get(&id).copied().unwrap_or([0.0, 0.0]),
                    args: v.node_args.get(&id).cloned().unwrap_or_default(),
                    running: app
                        .as_ref()
                        .map(|n| n.running.load(Ordering::Relaxed))
                        .unwrap_or(false),
                    compiling: app
                        .as_ref()
                        .is_some_and(|n| n.is_loading() && !n.finished.load(Ordering::Relaxed)),
                    runnable: app.as_ref().map(|n| n.is_runnable()).unwrap_or(false),
                    terminal: app.as_ref().map(|n| n.is_command()).unwrap_or(false),
                    attached: self.attached.contains(&id),
                    error: v.port_errors.get(&id).cloned(),
                    // The node's *effective* token: its custom one, else the
                    // workspace default — what `wk token` inspects/attenuates.
                    token: app.as_ref().and_then(|_| {
                        self.graph
                            .node_tokens
                            .get(&id)
                            .cloned()
                            .or_else(|| self.node_auth.as_ref().map(|(_, base)| base.clone()))
                            .map(|t| crate::workspace::bytes_hex(&t))
                    }),
                    // An uplink's ticket is the whole point of the node: the
                    // remote side can't dial without it, and it is otherwise
                    // only ever printed to the server's stderr at startup.
                    ticket: v.uplinks.get(&id).map(|u| u.ticket.clone()),
                    peers: v.uplinks.get(&id).map(|u| u.peers),
                    // Addressing a node across an uplink means using its
                    // fabric IP (names don't cross a trunk), and until now
                    // nothing reported it — you had to derive it from the
                    // node id by hand.
                    ip: fabric_addrs.get(&id).map(|(v4, _)| v4.clone()),
                    ip6: fabric_addrs.get(&id).map(|(_, v6)| v6.clone()),
                }
            })
            .collect();
        let wire = |kind: &'static str, pairs: &[(NodeId, NodeId)]| -> Vec<WireInfo> {
            pairs
                .iter()
                .map(|&(a, b)| WireInfo {
                    kind: kind.to_string(),
                    a,
                    b,
                })
                .collect()
        };
        let mut wires = wire("bind", &v.connections);
        wires.extend(wire("midi", &v.midi_links));
        wires.extend(wire("net", &v.net_links));
        wires.extend(wire("capture", &v.capture_links));
        wires.extend(wire("clipboard", &v.clipboard_links));
        wires.extend(wire("api", &v.api_links));
        wires.extend(v.serves.iter().map(|(&http, &hp)| WireInfo {
            kind: "serve".to_string(),
            a: http,
            b: hp,
        }));
        Snapshot {
            workspaces: v.workspaces.clone(),
            workspace_names: v.workspace_names.clone(),
            nodes,
            wires,
            available: v.available.iter().map(|d| d.name.clone()).collect(),
        }
    }
}

#[cfg(test)]
mod hostsvc_tests {
    use super::*;
    use std::io::{Read, Write};

    /// `bridge_to_host` splices an accepted fabric connection (its socketpair
    /// end) to a real host TCP server, both directions, with EOF propagating.
    #[test]
    fn bridge_reaches_host_service_and_shuttles_bytes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let target = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).unwrap();
            s.write_all(b"echo:").unwrap();
            s.write_all(&buf[..n]).unwrap();
        });

        let (fabric_end, ours) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut ours = ours;
        // Set before the bridge runs — see the sibling test.
        ours.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        bridge_to_host(fabric_end, target);
        ours.write_all(b"hello").unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < b"echo:hello".len() {
            let n = ours.read(&mut buf).expect("bytes come back");
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&got, b"echo:hello");
        server.join().unwrap();
    }

    /// A dead target drops the fabric connection instead of hanging it.
    #[test]
    fn bridge_to_dead_target_drops_the_connection() {
        // Bind-then-drop to get a port with nothing listening.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (fabric_end, ours) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut ours = ours;
        // Before the bridge runs: once its failed dial drops the far end,
        // macOS rejects setsockopt on the now-peerless socket with EINVAL.
        ours.set_read_timeout(Some(std::time::Duration::from_secs(15)))
            .unwrap();
        bridge_to_host(fabric_end, format!("127.0.0.1:{port}"));
        let mut buf = [0u8; 8];
        // EOF (Ok(0)) — the bridge closed its end after the failed dial.
        assert_eq!(ours.read(&mut buf).unwrap(), 0);
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

    /// The full node-token lifecycle against a live server: the default token
    /// allows exactly what is wired; an attenuated replacement narrows it (and
    /// its denial is honored live); reset returns to the default; a token from
    /// a foreign root is refused; undo restores the previous token.
    #[test]
    fn node_tokens_gate_wire_use_and_swap_live() {
        let mut s = fresh_server();
        let ws = s.graph.workspaces[0];
        let root = biscuit_auth::KeyPair::new();
        let base = biscuit_auth::Biscuit::builder()
            .code(wk_token_service::NODE_BASE_RULE)
            .unwrap()
            .build(&root)
            .unwrap()
            .to_vec()
            .unwrap();
        s.set_node_auth(root.public(), base);

        // An app node (a stand-in record is enough — authorization reads the
        // graph, not the wasm) wired to a volume and a network.
        let app = NodeId::new();
        s.place(app, Kind::App, ws, [0.0, 0.0], [100.0, 100.0]);
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Volume,
            pos: [0.0, 0.0],
            ws,
        }));
        let vol = *s.graph.file_nodes.keys().next().expect("volume placed");
        let net = NodeId::new();
        s.place(net, Kind::Network, ws, [0.0, 0.0], [80.0, 80.0]);
        s.graph.connections.push((vol, app));
        s.graph.net_links.push((app, net));

        // Default token: wired ⇒ usable (every action), unwired ⇒ not.
        assert!(s.node_may_use(app, "file", vol, "read"));
        assert!(s.node_may_use(app, "file", vol, "write"));
        assert!(s.node_may_use(app, "net", net, "use"));
        assert!(!s.node_may_use(app, "file", NodeId::new(), "read"));
        // Non-app nodes are exempt subjects (always allowed).
        assert!(s.node_may_use(net, "file", vol, "read"));

        // Attenuate: same authority, plus a check refusing file use. Swapped in
        // via the command path, the file grant dies and the net grant survives.
        let effective = s
            .graph
            .node_tokens
            .get(&app)
            .cloned()
            .unwrap_or_else(|| s.node_auth.as_ref().unwrap().1.clone());
        let attenuated = biscuit_auth::UnverifiedBiscuit::from(effective.as_slice())
            .unwrap()
            .append(
                biscuit_auth::builder::BlockBuilder::new()
                    .code(r#"check if operation($k, $t, $a), $k != "file" || $a == "read";"#)
                    .unwrap(),
            )
            .unwrap()
            .to_vec()
            .unwrap();
        s.apply(Command::SetToken {
            id: app,
            token: attenuated.clone(),
        });
        assert_eq!(s.graph.node_tokens.get(&app), Some(&attenuated));
        assert!(s.node_may_use(app, "file", vol, "read"), "reads survive");
        assert!(!s.node_may_use(app, "file", vol, "write"), "writes revoked");
        assert!(s.node_may_use(app, "net", net, "use"), "net use survives");

        // Undo restores the default token (write use returns).
        s.apply(Command::Undo);
        assert!(!s.graph.node_tokens.contains_key(&app));
        assert!(s.node_may_use(app, "file", vol, "write"));

        // A token minted by a different root is refused outright.
        let foreign = biscuit_auth::Biscuit::builder()
            .code(wk_token_service::NODE_BASE_RULE)
            .unwrap()
            .build(&biscuit_auth::KeyPair::new())
            .unwrap()
            .to_vec()
            .unwrap();
        s.apply(Command::SetToken {
            id: app,
            token: foreign,
        });
        assert!(!s.graph.node_tokens.contains_key(&app), "foreign refused");

        // Unwiring flips the decision (the memo respects the wire set).
        s.graph.connections.clear();
        assert!(!s.node_may_use(app, "file", vol, "read"), "no longer wired");

        // A gateway membership is its own kind: cutting off "gateway" keeps a
        // plain net working but severs host access.
        let gw = NodeId::new();
        s.place(gw, Kind::Gateway, ws, [0.0, 0.0], [80.0, 80.0]);
        s.graph.net_links.push((app, gw));
        assert!(s.node_may_use(app, "gateway", gw, "use"));
        let no_gw =
            biscuit_auth::UnverifiedBiscuit::from(s.node_auth.as_ref().unwrap().1.as_slice())
                .unwrap()
                .append(
                    biscuit_auth::builder::BlockBuilder::new()
                        .code(r#"check if operation($k, $t, $a), $k != "gateway";"#)
                        .unwrap(),
                )
                .unwrap()
                .to_vec()
                .unwrap();
        s.apply(Command::SetToken {
            id: app,
            token: no_gw,
        });
        assert!(
            !s.node_may_use(app, "gateway", gw, "use"),
            "host access cut"
        );
        assert!(s.node_may_use(app, "net", net, "use"), "plain net survives");

        // wk:scene: allowed by default with no wire at all; the mute
        // attenuation drops the node's entities out of the *view* (a live,
        // viewer-side mute — the entity itself stays registered) and reset
        // brings them back.
        s.host
            .scene_registry()
            .lock()
            .unwrap()
            .push(Arc::new(Mutex::new(crate::scene::EntityState {
                id: 1,
                node_id: app,
                glb: Arc::new(Vec::new()),
                glb_hash: 0,
                pos: [0.0; 3],
                yaw: 0.0,
                scale: 1.0,
                scenery: false,
                events: std::collections::VecDeque::new(),
            })));
        s.apply(Command::SetToken {
            id: app,
            token: Vec::new(), // reset to the default token
        });
        assert!(s.node_may_use(app, "scene", app, "show"));
        assert_eq!(s.view().scene_entities.len(), 1, "entity renders");
        let mute =
            biscuit_auth::UnverifiedBiscuit::from(s.node_auth.as_ref().unwrap().1.as_slice())
                .unwrap()
                .append(
                    biscuit_auth::builder::BlockBuilder::new()
                        .code(r#"check if operation($k, $t, $a), $k != "scene";"#)
                        .unwrap(),
                )
                .unwrap()
                .to_vec()
                .unwrap();
        s.apply(Command::SetToken {
            id: app,
            token: mute,
        });
        assert_eq!(s.view().scene_entities.len(), 0, "muted out of the view");
        assert_eq!(
            s.host.scene_entities().len(),
            1,
            "the entity itself stays registered (mute is viewer-side)"
        );
        s.apply(Command::SetToken {
            id: app,
            token: Vec::new(),
        });
        assert_eq!(s.view().scene_entities.len(), 1, "unmuted");
    }

    /// A boundary port in a plain tab: it places, wires by its declared kind,
    /// projects back to the file unchanged — and runs nothing. There is no
    /// other side of the boundary in a tab, so the whole node is inert.
    #[test]
    fn a_boundary_port_places_and_wires_but_runs_nothing_in_a_tab() {
        let mut s = fresh_server();
        let ws = s.graph.workspaces[0];
        let notes = NodeId::from_u128(101);
        let samples = NodeId::from_u128(102);
        let place_port = |s: &mut Server, id: NodeId, kind: SnapKind| {
            s.materialize(
                ws,
                &NodeSnap {
                    id,
                    pos: [0.0, 0.0],
                    size: [FILE_W, FILE_H],
                    pos3d: None,
                    panel3d: true,
                    kind,
                },
                &[],
            );
        };
        place_port(
            &mut s,
            notes,
            SnapKind::InPort {
                name: "notes".into(),
                kind: PortKind::Midi,
            },
        );
        place_port(
            &mut s,
            samples,
            SnapKind::OutPort {
                name: "samples".into(),
                kind: PortKind::Bind,
            },
        );
        // Materializing a port spawns no guest: it is a declaration, not a
        // program, and a tab full of them starts nothing.
        assert!(s.node_reg.lock().unwrap().is_empty());

        // An app node (a placed record is enough — wiring reads the graph) and
        // a volume, to wire the two ports to.
        let app = NodeId::from_u128(103);
        s.place(app, Kind::App, ws, [0.0, 0.0], [100.0, 100.0]);
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Volume,
            pos: [0.0, 0.0],
            ws,
        }));
        let vol = *s.graph.file_nodes.keys().next().expect("volume placed");

        // Each port wires as the kind it declares, in the orientation an
        // expansion can collapse: the in-port is the MIDI *source*, the
        // out-port the bind's *destination*.
        s.apply(Command::Create(Resource::Wire { a: app, b: notes }));
        assert_eq!(s.graph.midi_links, vec![(notes, app)]);
        s.apply(Command::Create(Resource::Wire { a: vol, b: samples }));
        assert_eq!(s.graph.connections, vec![(vol, samples)]);
        // Nothing was actually mounted — there is no filesystem behind a port.
        assert!(s.mounted.is_empty());

        // A wire of the wrong kind is refused outright, not reclassified: the
        // MIDI port meeting a volume must not become a bind.
        s.apply(Command::Create(Resource::Wire { a: vol, b: notes }));
        assert_eq!(s.graph.connections, vec![(vol, samples)], "no new bind");
        assert_eq!(s.graph.midi_links, vec![(notes, app)], "and no new route");

        // Both ports project back to exactly what the file said.
        assert_eq!(
            s.node_snap(notes).expect("in-port projects").kind,
            SnapKind::InPort {
                name: "notes".into(),
                kind: PortKind::Midi,
            }
        );
        assert_eq!(
            s.node_snap(samples).expect("out-port projects").kind,
            SnapKind::OutPort {
                name: "samples".into(),
                kind: PortKind::Bind,
            }
        );

        // Deleting one takes its wires with it rather than leaving them
        // dangling against an id nothing can resolve.
        s.apply(Command::Delete(ResourceRef::Node(samples)));
        assert!(s.graph.connections.is_empty());
        assert!(!s.graph.boundary_ports.contains_key(&samples));
    }

    /// A group node with the definition it names, as a whole document. The
    /// definition is a named Volume behind an in-port — nothing that needs
    /// wasm, so the whole instance materializes in a plain unit test.
    fn instancing_doc(definition: &str, tab: NodeId, inst: NodeId) -> Document {
        use crate::workspace::{NodeSnap, SnapKind};
        let snap = |id: NodeId, kind: SnapKind| NodeSnap {
            id,
            pos: [10.0, 20.0],
            size: [200.0, 120.0],
            pos3d: None,
            panel3d: true,
            kind,
        };
        let (port, vol) = (NodeId::from_u128(0xF01), NodeId::from_u128(0xF02));
        Document {
            workspaces: vec![
                Workspace {
                    id: NodeId::from_u128(0xF00),
                    name: Some("voice".into()),
                    tab: false,
                    nodes: vec![
                        snap(
                            port,
                            SnapKind::InPort {
                                name: "notes".into(),
                                kind: PortKind::Midi,
                            },
                        ),
                        snap(
                            vol,
                            SnapKind::Volume {
                                name: "chan".into(),
                                persist: false,
                            },
                        ),
                    ],
                    ..Workspace::new()
                },
                Workspace {
                    id: tab,
                    nodes: vec![snap(
                        inst,
                        SnapKind::Group {
                            definition: definition.to_string(),
                            name: None,
                            in_wires: Vec::new(),
                            out_wires: Vec::new(),
                        },
                    )],
                    ..Workspace::new()
                },
            ],
            ..Document::empty()
        }
    }

    /// A `group` runs its definition — and none of what it runs may reach the
    /// `.wk` file. The instance's nodes are derived: they exist because the
    /// definition says so, and writing them back would turn one instance into
    /// two on the next load. So load → run → save must hand the file back with
    /// nothing but the group line in it.
    #[test]
    fn a_group_runs_its_definition_without_writing_any_of_it_back() {
        let path = std::env::temp_dir().join("wk-group-save-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, inst) = (NodeId::new(), NodeId::new());
        let doc = instancing_doc("voice", tab, inst);
        let s = Server::new(&doc, path.clone()).expect("server constructs");

        // The group itself is a node on the tab's canvas...
        assert_eq!(s.kind_of(inst), Some(Kind::Group));
        // ...and the definition's HostPort is live under a derived id, in the
        // instance rather than in the tab. The in-port is not: a boundary port
        // is a marker on the definition's canvas, never a runtime thing.
        let derived: Vec<NodeId> = s.instances[&inst].nodes.clone();
        assert_eq!(derived.len(), 1, "one node: the volume, not the in-port");
        assert_eq!(s.kind_of(derived[0]), Some(Kind::File));
        assert_eq!(
            s.graph.nodes[&derived[0]].ws, inst,
            "it belongs to the instance"
        );
        assert!(
            !s.graph.workspaces.contains(&inst),
            "an instance is not a tab"
        );

        s.save();
        let back = Document::load(&path).expect("reloads");
        let saved = back
            .workspaces
            .iter()
            .find(|w| w.id == tab)
            .expect("the tab is still there");
        assert_eq!(
            saved.nodes.len(),
            1,
            "only the group line: {:?}",
            saved.nodes
        );
        assert_eq!(saved.nodes[0].id, inst);
        assert_eq!(
            Document::load(&path).expect("reloads"),
            back,
            "a second save/load cycle is a fixpoint"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A document whose instancing can't resolve must refuse to start rather
    /// than run the half of it that does — the missing half would be saved back
    /// as deliberately gone.
    #[test]
    fn a_group_naming_no_definition_refuses_to_start() {
        let path = std::env::temp_dir().join("wk-group-broken-test.wk");
        let _ = std::fs::remove_file(&path);
        let doc = instancing_doc("vioce", NodeId::new(), NodeId::new());
        let err = match Server::new(&doc, path.clone()) {
            Err(e) => e,
            Ok(_) => panic!("a group naming no definition must refuse to start"),
        };
        assert!(err.contains("\"vioce\""), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    /// Two groups of one definition are two independent sets of live nodes —
    /// the point of instancing. Their ids are derived from the instance, so
    /// nothing is shared and nothing collides.
    #[test]
    fn two_instances_of_a_definition_run_independently() {
        let path = std::env::temp_dir().join("wk-group-twice-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, first, second) = (NodeId::new(), NodeId::new(), NodeId::new());
        let mut doc = instancing_doc("voice", tab, first);
        let twin = doc.workspaces[1].nodes[0].clone();
        doc.workspaces[1]
            .nodes
            .push(crate::workspace::NodeSnap { id: second, ..twin });
        let mut s = Server::new(&doc, path.clone()).expect("server constructs");
        let a = s.instances[&first].nodes.clone();
        let b = s.instances[&second].nodes.clone();
        assert_eq!((a.len(), b.len()), (1, 1));
        assert_ne!(a[0], b[0], "two instances must not share a node");

        // `wk ps` has to tell them apart, or neither can be addressed.
        let snap = s.ipc_snapshot();
        // Sorted, because a snapshot's node order is the live table's, not the
        // file's — what matters is that the two labels differ and say which
        // instance each node is in.
        let mut names: Vec<&str> = snap
            .nodes
            .iter()
            .filter(|n| n.kind == "volume")
            .map(|n| n.name.as_str())
            .collect();
        names.sort();
        // A volume has no name of its own — only a role inside its instance —
        // so `wk ps` qualifies it with the instance that placed it, and the
        // two instances' names differ because each is derived from its own id.
        let scopes: Vec<&str> = names.iter().map(|n| n.split('/').next().unwrap()).collect();
        assert_ne!(scopes[0], scopes[1], "two instances, two scopes: {names:?}");
        assert!(
            names.iter().all(|n| n.ends_with("/chan")),
            "each is still the definition's `chan`: {names:?}"
        );
        // And each derived node reports the *tab* it is shown in, since an
        // instance is not a workspace the CLI could name.
        assert!(snap.nodes.iter().all(|n| n.ws == tab));
        let _ = std::fs::remove_file(&path);
    }

    /// Deleting the `group` node tears the instance down. Anything less leaves
    /// live guests running with nothing on the canvas to reach them — and undo
    /// must bring the whole instance back in ONE press, not one per node.
    #[test]
    fn deleting_a_group_tears_its_instance_down_and_undo_is_one_step() {
        let path = std::env::temp_dir().join("wk-group-delete-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, inst) = (NodeId::new(), NodeId::new());
        let mut s =
            Server::new(&instancing_doc("voice", tab, inst), path.clone()).expect("constructs");
        let derived = s.instances[&inst].nodes.clone();
        assert!(derived.iter().all(|&id| s.node_exists(id)));

        s.apply(Command::Delete(ResourceRef::Node(inst)));
        assert!(!s.node_exists(inst));
        assert!(s.instances.is_empty(), "the instance outlived its group");
        assert!(derived.iter().all(|&id| !s.node_exists(id)));

        s.apply(Command::Undo);
        assert!(s.node_exists(inst), "one undo brings the group back");
        assert!(
            derived.iter().all(|&id| s.node_exists(id)),
            "...and everything it stands for, in the same press"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Duplicating a group is how a second instance is made on the canvas: the
    /// same definition, a new id, and therefore its own nodes. Undo must take
    /// the whole copy back in ONE press — the create produced a group node
    /// *and* everything under it, and one entry per node would leave a
    /// half-dismantled live instance between presses.
    #[test]
    fn duplicating_a_group_makes_a_second_instance_and_undo_is_one_step() {
        let path = std::env::temp_dir().join("wk-group-duplicate-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, inst) = (NodeId::new(), NodeId::new());
        let mut s =
            Server::new(&instancing_doc("voice", tab, inst), path.clone()).expect("constructs");
        let before: HashSet<NodeId> = s.graph.nodes.keys().copied().collect();

        s.apply(Command::Duplicate(inst));
        let added: Vec<NodeId> = s
            .graph
            .nodes
            .keys()
            .copied()
            .filter(|id| !before.contains(id))
            .collect();
        assert_eq!(added.len(), 2, "a group node plus the volume it stands for");
        assert_eq!(s.instances.len(), 2, "two independent instances");
        let originals = s.instances[&inst].nodes.clone();
        assert!(
            added.iter().all(|id| !originals.contains(id)),
            "the copy must not share a node with the original"
        );

        s.apply(Command::Undo);
        assert!(added.iter().all(|&id| !s.node_exists(id)));
        assert_eq!(s.instances.len(), 1, "one press, one instance removed");
        assert!(originals.iter().all(|&id| s.node_exists(id)));
        let _ = std::fs::remove_file(&path);
    }

    /// A structural edit inside an instance is refused server-side. Its nodes
    /// are derived, so a delete there would be undone by the next restart —
    /// silently, after the user had already built on it.
    #[test]
    fn an_instances_nodes_are_read_only_for_structure() {
        let path = std::env::temp_dir().join("wk-group-readonly-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, inst) = (NodeId::new(), NodeId::new());
        let mut s =
            Server::new(&instancing_doc("voice", tab, inst), path.clone()).expect("constructs");
        let derived = s.instances[&inst].nodes[0];

        s.apply(Command::Delete(ResourceRef::Node(derived)));
        assert!(s.node_exists(derived), "a derived node was deleted");
        s.apply(Command::Duplicate(derived));
        assert_eq!(
            s.instances[&inst].nodes.len(),
            1,
            "a derived node was copied"
        );
        // Nor may a node be added to the instance's canvas: there is nowhere
        // in the file to write it down.
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Network,
            pos: [0.0, 0.0],
            ws: inst,
        }));
        assert!(
            s.graph.nodes.values().all(|r| r.kind != Kind::Network),
            "a node was added to an instance"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A boundary port never becomes a live node: the parent's wire *into* the
    /// port and the definition's wire *out* of it collapse into one ordinary
    /// wire, from the parent's own node straight to the instance's. And that
    /// wire must not reach the file — its far end is a derived id, so writing
    /// it would leave the next load with a wire to a node that isn't there yet.
    ///
    /// Uses a Note as the granted node: it classifies like an app without
    /// needing a compiled component, so the whole path runs wasm-free.
    #[test]
    fn a_wire_crossing_a_boundary_collapses_and_stays_out_of_the_file() {
        use crate::workspace::{NodeSnap, SnapKind};
        let path = std::env::temp_dir().join("wk-group-collapse-test.wk");
        let _ = std::fs::remove_file(&path);
        let snap = |id: NodeId, kind: SnapKind| NodeSnap {
            id,
            pos: [0.0, 0.0],
            size: [130.0, 44.0],
            pos3d: None,
            panel3d: true,
            kind,
        };
        let (def, tab) = (NodeId::from_u128(0xD00), NodeId::from_u128(0xD01));
        let (port, note) = (NodeId::from_u128(0xD02), NodeId::from_u128(0xD03));
        let (cap, inst) = (NodeId::from_u128(0xD04), NodeId::from_u128(0xD05));
        let doc = Document {
            workspaces: vec![
                Workspace {
                    id: def,
                    name: Some("viewer".into()),
                    tab: false,
                    nodes: vec![
                        snap(
                            port,
                            SnapKind::InPort {
                                name: "frames".into(),
                                kind: PortKind::Capture,
                            },
                        ),
                        snap(note, SnapKind::Note { text: "eye".into() }),
                    ],
                    capture_links: vec![(note, port)],
                    ..Workspace::new()
                },
                Workspace {
                    id: tab,
                    nodes: vec![
                        snap(cap, SnapKind::Capture),
                        snap(
                            inst,
                            SnapKind::Group {
                                definition: "viewer".into(),
                                name: None,
                                in_wires: vec![("frames".into(), cap)],
                                out_wires: Vec::new(),
                            },
                        ),
                    ],
                    ..Workspace::new()
                },
            ],
            ..Document::empty()
        };
        let s = Server::new(&doc, path.clone()).expect("constructs");
        let derived = s.instances[&inst].nodes[0];
        assert_eq!(
            s.kind_of(derived),
            Some(Kind::Note),
            "the note materialized"
        );
        assert!(
            !s.node_exists(crate::instancing::derive_id(inst, port)),
            "the boundary port must not become a live node"
        );
        assert_eq!(
            s.graph.capture_links,
            vec![(derived, cap)],
            "the grant should run from the instance's note to the tab's Capture node"
        );

        s.save();
        let back = Document::load(&path).expect("reloads");
        let saved = back.workspaces.iter().find(|w| w.id == tab).expect("tab");
        assert!(
            saved.capture_links.is_empty(),
            "a collapsed wire was written to the file: {:?}",
            saved.capture_links
        );
        assert_eq!(saved.nodes.len(), 2, "the Capture node and the group, only");
        let _ = std::fs::remove_file(&path);
    }

    /// A definition whose in-port grants *two* inner nodes, and a tab holding
    /// one instance of it, the Capture node that may cross its port, and a
    /// Network node that may not. `wired` writes the `in "frames"` line into
    /// the group's block, or leaves the instance unwired for a test that wires
    /// it through the canvas.
    ///
    /// Notes stand in for apps throughout: they classify the same way without
    /// needing a compiled component, so the whole path runs wasm-free.
    /// The fixed ids [`boundary_doc`] places, so a test can name what it wires
    /// to what. Fixed rather than minted because a derived id is a function of
    /// the group's own id, and a failure that prints one should be readable.
    struct BIds {
        def: NodeId,
        tab: NodeId,
        port: NodeId,
        eyes: [NodeId; 2],
        cap: NodeId,
        net: NodeId,
        inst: NodeId,
    }

    fn bids() -> BIds {
        BIds {
            def: NodeId::from_u128(0xB00),
            tab: NodeId::from_u128(0xB01),
            port: NodeId::from_u128(0xB02),
            eyes: [NodeId::from_u128(0xB03), NodeId::from_u128(0xB04)],
            cap: NodeId::from_u128(0xB05),
            net: NodeId::from_u128(0xB06),
            inst: NodeId::from_u128(0xB07),
        }
    }

    fn boundary_doc(wired: bool) -> Document {
        use crate::workspace::{NodeSnap, SnapKind};
        let snap = |id: NodeId, kind: SnapKind| NodeSnap {
            id,
            pos: [0.0, 0.0],
            size: [130.0, 44.0],
            pos3d: None,
            panel3d: true,
            kind,
        };
        let note = |id: NodeId, text: &str| {
            snap(
                id,
                SnapKind::Note {
                    text: text.to_string(),
                },
            )
        };
        let b = bids();
        Document {
            workspaces: vec![
                Workspace {
                    id: b.def,
                    name: Some("viewer".into()),
                    tab: false,
                    nodes: vec![
                        snap(
                            b.port,
                            SnapKind::InPort {
                                name: "frames".into(),
                                kind: PortKind::Capture,
                            },
                        ),
                        note(b.eyes[0], "eye"),
                        note(b.eyes[1], "other eye"),
                    ],
                    // One port feeding two nodes: the fan-out a boundary wire
                    // has to produce, rather than one wire to whichever came
                    // first.
                    capture_links: vec![(b.eyes[0], b.port), (b.eyes[1], b.port)],
                    ..Workspace::new()
                },
                Workspace {
                    id: b.tab,
                    nodes: vec![
                        snap(b.cap, SnapKind::Capture),
                        snap(b.net, SnapKind::Net { gateway: false }),
                        snap(
                            b.inst,
                            SnapKind::Group {
                                definition: "viewer".into(),
                                name: None,
                                in_wires: if wired {
                                    vec![("frames".into(), b.cap)]
                                } else {
                                    Vec::new()
                                },
                                out_wires: Vec::new(),
                            },
                        ),
                    ],
                    ..Workspace::new()
                },
            ],
            ..Document::empty()
        }
    }

    /// The client's gesture: dragging a wire onto an instance's port disc.
    /// What it authors is a line in the group's block — the instance has no
    /// wireable node of its own — and what that line *does* is decided by
    /// re-expanding: here one drag makes two live grants, because the
    /// definition's port feeds two nodes. It is still one edit and one undo.
    #[test]
    fn wiring_an_instances_port_authors_a_boundary_wire_that_one_undo_takes_back() {
        let path = std::env::temp_dir().join("wk-boundary-author-test.wk");
        let _ = std::fs::remove_file(&path);
        let b = bids();
        let mut s = Server::new(&boundary_doc(false), path.clone()).expect("constructs");
        let derived = s.instances[&b.inst].nodes.clone();
        assert_eq!(derived.len(), 2, "both inner nodes ran");
        assert!(
            s.graph.capture_links.is_empty(),
            "an unwired port grants nothing"
        );

        let bw = BoundaryWire {
            group: b.inst,
            dir: PortDir::In,
            port: "frames".into(),
            node: b.cap,
        };
        s.apply(Command::Create(Resource::Boundary(bw.clone())));
        assert_eq!(
            s.graph.groups[&b.inst].in_wires,
            vec![("frames".to_string(), b.cap)]
        );
        assert_eq!(
            s.graph.capture_links,
            vec![(derived[0], b.cap), (derived[1], b.cap)],
            "one boundary wire, one grant per node the port reaches"
        );
        // The client is told what this canvas authored, because the live wire
        // it became ends inside the instance where no tab can see it.
        assert_eq!(
            s.view().groups[&b.inst].in_wires,
            vec![("frames".to_string(), b.cap)]
        );
        // It is the *file's* line: the group block carries it and the
        // collapsed grants stay out.
        s.save();
        let back = Document::load(&path).expect("reloads");
        let tab = back.workspaces.iter().find(|w| w.id == b.tab).expect("tab");
        assert!(tab.capture_links.is_empty(), "{:?}", tab.capture_links);
        let group = tab.nodes.iter().find(|n| n.id == b.inst).expect("group");
        assert_eq!(
            group.kind,
            crate::workspace::SnapKind::Group {
                definition: "viewer".into(),
                name: None,
                in_wires: vec![("frames".into(), b.cap)],
                out_wires: Vec::new(),
            }
        );

        // Dragging the same pair again takes the line away, grants and all...
        s.apply(Command::Delete(ResourceRef::Boundary(bw)));
        assert!(s.graph.groups[&b.inst].in_wires.is_empty());
        assert!(s.graph.capture_links.is_empty());
        // ...and one Ctrl-Z restores it — one entry per line the user drew,
        // never one per wire the expansion moved.
        s.apply(Command::Undo);
        assert_eq!(s.graph.capture_links.len(), 2);
        s.apply(Command::Undo);
        assert!(s.graph.capture_links.is_empty());
        assert!(s.graph.groups[&b.inst].in_wires.is_empty());
        // Through all of it the instance kept running: a boundary wire decides
        // an instance's wiring, never which nodes it has.
        assert_eq!(s.instances[&b.inst].nodes, derived);
        let _ = std::fs::remove_file(&path);
    }

    /// Deleting the node a boundary wire names takes the line with it — and
    /// undoing brings both back.
    ///
    /// A dangling line is not a cosmetic problem: an `in`/`out` line whose far
    /// end is not on the canvas is refused at load, so leaving it behind would
    /// write a `.wk` file that no longer starts.
    #[test]
    fn deleting_a_wired_neighbour_takes_the_boundary_wire_with_it_and_undo_restores_both() {
        let path = std::env::temp_dir().join("wk-boundary-delete-test.wk");
        let _ = std::fs::remove_file(&path);
        let b = bids();
        let mut s = Server::new(&boundary_doc(true), path.clone()).expect("constructs");
        assert_eq!(s.graph.capture_links.len(), 2, "the instance starts wired");

        s.apply(Command::Delete(ResourceRef::Node(b.cap)));
        assert!(
            s.graph.groups[&b.inst].in_wires.is_empty(),
            "the line outlived the node it named"
        );
        assert!(s.graph.capture_links.is_empty());
        // What the file now says must still load, or the next start refuses.
        s.save();
        let back = Document::load(&path).expect("reloads");
        Server::new(&back, path.clone()).expect("the saved document still starts");

        s.apply(Command::Undo);
        assert!(s.node_exists(b.cap));
        assert_eq!(
            s.graph.groups[&b.inst].in_wires,
            vec![("frames".to_string(), b.cap)],
            "undo brought the node back unwired"
        );
        assert_eq!(s.graph.capture_links.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// A boundary wire the expansion won't have is refused whole: the group is
    /// left exactly as it was, still expanded, rather than holding a line that
    /// no longer resolves.
    #[test]
    fn a_boundary_wire_the_expansion_refuses_leaves_the_group_as_it_was() {
        let path = std::env::temp_dir().join("wk-boundary-refuse-test.wk");
        let _ = std::fs::remove_file(&path);
        let b = bids();
        let mut s = Server::new(&boundary_doc(false), path.clone()).expect("constructs");
        let derived = s.instances[&b.inst].nodes.clone();
        let wire = |port: &str, node| {
            Command::Create(Resource::Boundary(BoundaryWire {
                group: b.inst,
                dir: PortDir::In,
                port: port.to_string(),
                node,
            }))
        };
        // A port the definition does not declare...
        s.apply(wire("frame", b.cap));
        // ...and a neighbour that cannot be the source of a `capture` wire.
        s.apply(wire("frames", b.net));
        assert!(
            s.graph.groups[&b.inst].in_wires.is_empty(),
            "a refused line was written into the group"
        );
        assert!(s.graph.capture_links.is_empty());
        assert_eq!(s.instances[&b.inst].nodes, derived, "the instance survived");
        // Nothing was recorded either, so Ctrl-Z does not undo the last real
        // edit twice.
        assert!(s.undo.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// A boundary wire is authored state, and `save` is a full re-projection
    /// from the live graph — so the group's `in` line, its comment, and the
    /// definition it names must all come back byte for byte, both after a
    /// plain load → run → save and after an edit somewhere else in the
    /// document (what `wk node add` does: a `Create` through the same path).
    #[test]
    fn a_boundary_wire_survives_a_save_and_an_unrelated_edit() {
        let path = std::env::temp_dir().join("wk-boundary-file-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, def) = (NodeId::from_u128(0xC00), NodeId::from_u128(0xC01));
        let (cap, inst) = (NodeId::from_u128(0xC02), NodeId::from_u128(0xC03));
        let (port, eye) = (NodeId::from_u128(0xC04), NodeId::from_u128(0xC05));
        let original = format!(
            "workspace \"{tab}\" {{\n    \
               capture \"{cap}\" {{ pos 0 0; size 130 44 }}\n    \
               group \"viewer\" \"{inst}\" {{\n        \
                 // the screen this viewer watches\n        \
                 in \"frames\" \"{cap}\"\n        \
                 pos 10 20\n        size 200 120\n    }}\n}}\n\
             workspace \"{def}\" {{\n    \
               name \"viewer\"\n    tab #false\n    \
               inport \"frames\" \"capture\" \"{port}\" {{ pos 0 0; size 130 44 }}\n    \
               note \"{eye}\" {{ text \"eye\"; pos 0 0; size 130 44 }}\n    \
               capturelink \"{eye}\" \"{port}\"\n}}\n"
        );
        // Normalize once through the writer so the comparison is about content
        // rather than whitespace any save at all would rewrite.
        std::fs::write(&path, &original).unwrap();
        Document::load(&path).expect("parses").save(&path).unwrap();
        let normalized = std::fs::read_to_string(&path).unwrap();
        assert!(
            normalized.contains("// the screen this viewer watches"),
            "the fixture carries the comment that must survive:\n{normalized}"
        );

        let doc = Document::load(&path).expect("loads");
        let mut s = Server::new(&doc, path.clone()).expect("constructs");
        assert_eq!(
            s.graph.capture_links.len(),
            1,
            "the boundary wire is live: the instance's note is granted the tab's Capture"
        );
        s.save();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            normalized,
            "the group's block did not come back unchanged"
        );

        // An edit elsewhere re-projects the whole document. The `in` line has
        // no live wire of its own to be projected *from*, so it survives only
        // because it is carried on the group node.
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Note,
            pos: [400.0, 400.0],
            ws: tab,
        }));
        s.save();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains(&format!("in \"frames\" \"{cap}\"")),
            "the boundary wire was deleted by an unrelated edit:\n{after}"
        );
        assert!(
            after.contains("// the screen this viewer watches"),
            "{after}"
        );
        assert!(
            after.contains("name \"viewer\"") && after.contains("tab #false"),
            "{after}"
        );
        assert!(
            !after.contains("capturelink \"01"),
            "a collapsed wire reached the tab:\n{after}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A boundary port on the *source* side of a one-per-source relation is
    /// refused, because the collapse would hand an outer node a second grant of
    /// something it may only have one of — and `toggle_unique` resolves that by
    /// dropping whatever the outer node already had. That link is authored
    /// content, and `save` re-projects the live graph, so the silent
    /// displacement takes the user's `capturelink` line out of their file. Two
    /// instances of one definition would steal it from each other the same way.
    #[test]
    fn a_port_that_would_move_an_outer_nodes_grant_is_refused() {
        use crate::workspace::{NodeSnap, SnapKind};
        let path = std::env::temp_dir().join("wk-group-displace-test.wk");
        let _ = std::fs::remove_file(&path);
        let snap = |id: NodeId, kind: SnapKind| NodeSnap {
            id,
            pos: [0.0, 0.0],
            size: [130.0, 44.0],
            pos3d: None,
            panel3d: true,
            kind,
        };
        let (def, tab) = (NodeId::from_u128(0xE00), NodeId::from_u128(0xE01));
        let (port, inner_cap) = (NodeId::from_u128(0xE02), NodeId::from_u128(0xE03));
        let (app, own_cap, inst) = (
            NodeId::from_u128(0xE04),
            NodeId::from_u128(0xE05),
            NodeId::from_u128(0xE06),
        );
        let doc = Document {
            workspaces: vec![
                Workspace {
                    id: def,
                    name: Some("viewer".into()),
                    tab: false,
                    nodes: vec![
                        snap(
                            port,
                            SnapKind::InPort {
                                name: "screen".into(),
                                kind: PortKind::Capture,
                            },
                        ),
                        snap(inner_cap, SnapKind::Capture),
                    ],
                    // The port stands in for an app *outside*, granted the
                    // instance's own Capture node.
                    capture_links: vec![(port, inner_cap)],
                    ..Workspace::new()
                },
                Workspace {
                    id: tab,
                    nodes: vec![
                        snap(app, SnapKind::Note { text: "app".into() }),
                        snap(own_cap, SnapKind::Capture),
                        snap(
                            inst,
                            SnapKind::Group {
                                definition: "viewer".into(),
                                name: None,
                                in_wires: vec![("screen".into(), app)],
                                out_wires: Vec::new(),
                            },
                        ),
                    ],
                    // ...which is also wired to a Capture node of its own.
                    capture_links: vec![(app, own_cap)],
                    ..Workspace::new()
                },
            ],
            ..Document::empty()
        };
        let err = match Server::new(&doc, path.clone()) {
            Err(e) => e,
            Ok(s) => panic!(
                "a port that moves an outer grant must be refused; capture links: {:?}",
                s.graph.capture_links
            ),
        };
        assert!(err.contains("\"screen\""), "names the port: {err}");
        assert!(err.contains("capture"), "names the relation: {err}");
        let _ = std::fs::remove_file(&path);
    }

    /// A definition may pass a connection straight through, in one port and out
    /// the complementary one. Both ends of the resulting wire are then nodes of
    /// the *parent*, so nothing about the endpoints says the expansion made it —
    /// and a wire written to the file would outlive the group that stands for
    /// it, indistinguishable from something the author drew.
    #[test]
    fn a_pass_through_definition_does_not_write_its_wire_into_the_file() {
        use crate::workspace::{NodeSnap, SnapKind};
        let path = std::env::temp_dir().join("wk-group-passthrough-test.wk");
        let _ = std::fs::remove_file(&path);
        let snap = |id: NodeId, kind: SnapKind| NodeSnap {
            id,
            pos: [0.0, 0.0],
            size: [130.0, 44.0],
            pos3d: None,
            panel3d: true,
            kind,
        };
        let (def, tab) = (NodeId::from_u128(0xC00), NodeId::from_u128(0xC01));
        let (pin, pout) = (NodeId::from_u128(0xC02), NodeId::from_u128(0xC03));
        let (src, dst, inst) = (
            NodeId::from_u128(0xC04),
            NodeId::from_u128(0xC05),
            NodeId::from_u128(0xC06),
        );
        let doc = Document {
            workspaces: vec![
                Workspace {
                    id: def,
                    name: Some("thru".into()),
                    tab: false,
                    nodes: vec![
                        snap(
                            pin,
                            SnapKind::InPort {
                                name: "from".into(),
                                kind: PortKind::Midi,
                            },
                        ),
                        snap(
                            pout,
                            SnapKind::OutPort {
                                name: "to".into(),
                                kind: PortKind::Midi,
                            },
                        ),
                    ],
                    midi: vec![(pin, pout)],
                    ..Workspace::new()
                },
                Workspace {
                    id: tab,
                    nodes: vec![
                        snap(
                            src,
                            SnapKind::Note {
                                text: "keys".into(),
                            },
                        ),
                        snap(
                            dst,
                            SnapKind::Note {
                                text: "synth".into(),
                            },
                        ),
                        snap(
                            inst,
                            SnapKind::Group {
                                definition: "thru".into(),
                                name: None,
                                in_wires: vec![("from".into(), src)],
                                out_wires: vec![("to".into(), dst)],
                            },
                        ),
                    ],
                    ..Workspace::new()
                },
            ],
            ..Document::empty()
        };
        let mut s = Server::new(&doc, path.clone()).expect("constructs");
        assert_eq!(
            s.graph.midi_links,
            vec![(src, dst)],
            "the pass-through should join the tab's own two nodes"
        );

        s.save();
        let back = Document::load(&path).expect("reloads");
        let saved = back.workspaces.iter().find(|w| w.id == tab).expect("tab");
        assert!(
            saved.midi.is_empty(),
            "the expansion's wire was written into the file as if the author drew it: {:?}",
            saved.midi
        );

        // And deleting the group must take the wire with it: the instance's own
        // nodes are not on either end, so nothing else can.
        s.apply(Command::Delete(ResourceRef::Node(inst)));
        assert!(
            s.graph.midi_links.is_empty(),
            "the pass-through wire outlived the group that made it"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A persisted volume inside a definition keeps its bytes per *instance*.
    /// Its sidecar is named by the derived id, and the prune keep-set is built
    /// from the document about to be written — which by design holds no derived
    /// node at all. Getting this wrong deletes the user's data on the way out.
    #[test]
    fn a_persisted_volume_inside_an_instance_keeps_its_sidecar() {
        let path = std::env::temp_dir().join("wk-group-volume-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, inst) = (NodeId::new(), NodeId::new());
        let mut doc = instancing_doc("voice", tab, inst);
        let def = &mut doc.workspaces[0];
        if let SnapKind::Volume { persist, .. } = &mut def.nodes[1].kind {
            *persist = true;
        }
        let s = Server::new(&doc, path.clone()).expect("constructs");
        let derived = s.instances[&inst].nodes[0];
        let _ = std::fs::remove_dir_all(s.volume_dir());
        std::fs::create_dir_all(s.volume_dir()).unwrap();
        std::fs::write(s.volume_sidecar(derived), b"remember me").unwrap();
        // A sidecar for a volume no instance claims any more is still pruned.
        let stale = s.volume_dir().join(NodeId::from_u128(0xDEAD).to_string());
        std::fs::write(&stale, b"nobody's").unwrap();

        s.save();
        assert!(
            s.volume_sidecar(derived).exists(),
            "an instance's volume lost its bytes at shutdown"
        );
        assert!(!stale.exists(), "a sidecar nothing claims is still pruned");
        let _ = std::fs::remove_dir_all(s.volume_dir());
        let _ = std::fs::remove_file(&path);
    }

    /// Closing the tab a group sits in must take the instance with it. The
    /// derived nodes are not in the tab (their workspace is the instance), so
    /// the tab teardown reaches them only through the group node.
    #[test]
    fn closing_a_tab_tears_down_the_instances_it_holds() {
        let path = std::env::temp_dir().join("wk-group-tab-close-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, inst) = (NodeId::new(), NodeId::new());
        let mut doc = instancing_doc("voice", tab, inst);
        // A second tab, so closing the first isn't the "keep one" no-op.
        doc.workspaces.push(Workspace::new());
        let mut s = Server::new(&doc, path.clone()).expect("constructs");
        let derived = s.instances[&inst].nodes.clone();

        s.apply(Command::Delete(ResourceRef::Workspace(tab)));
        assert!(s.instances.is_empty(), "the instance outlived its tab");
        assert!(derived.iter().all(|&id| !s.node_exists(id)));

        // And undoing the close brings the whole instance back with the tab.
        s.apply(Command::Undo);
        assert!(s.node_exists(inst));
        assert!(derived.iter().all(|&id| s.node_exists(id)));
        let _ = std::fs::remove_file(&path);
    }

    /// Undoing a *moved* one-per-source wire restores the displaced link, not
    /// just drops the new one. An uplink joins net1, then is rewired to net2
    /// (membership moves, net1 link displaced); undo must return it to net1.
    /// Uses uplink nodes since they wire to Networks without needing wasm.
    #[test]
    fn undo_of_a_moved_membership_restores_the_previous_net() {
        let mut s = fresh_server();
        let ws = s.graph.workspaces[0];
        let add_net = |s: &mut Server| {
            s.apply(Command::Create(Resource::Node {
                kind: NodeKind::Network,
                pos: [0.0, 0.0],
                ws,
            }));
        };
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Iroh,
            pos: [0.0, 0.0],
            ws,
        }));
        add_net(&mut s);
        add_net(&mut s);
        let uplink = *s.graph.iroh_secrets.keys().next().expect("uplink");
        let mut nets: Vec<NodeId> = s
            .graph
            .nodes
            .iter()
            .filter(|(_, r)| r.kind == Kind::Network)
            .map(|(&id, _)| id)
            .collect();
        nets.sort();
        let (net1, net2) = (nets[0], nets[1]);

        s.apply(Command::Create(Resource::Wire { a: uplink, b: net1 }));
        assert!(s.graph.net_links.contains(&(uplink, net1)));
        // Move membership to net2 — displaces the net1 link.
        s.apply(Command::Create(Resource::Wire { a: uplink, b: net2 }));
        assert!(s.graph.net_links.contains(&(uplink, net2)));
        assert!(!s.graph.net_links.contains(&(uplink, net1)));

        // Undo the move: back on net1, not left isolated.
        s.apply(Command::Undo);
        assert!(
            s.graph.net_links.contains(&(uplink, net1)),
            "undo restored the displaced net1 membership"
        );
        assert!(!s.graph.net_links.contains(&(uplink, net2)));
    }

    /// `View::for_workspace` keeps only the nodes belonging to that tab (across
    /// the id-keyed maps and node-set projections).
    #[test]
    fn for_workspace_isolates_each_tab() {
        let mut s = fresh_server();
        let ws1 = s.graph.workspaces[0];
        let ws2 = NodeId::new();
        s.apply(Command::Create(Resource::Workspace { id: ws2 }));
        let add = |s: &mut Server, kind, ws| {
            s.apply(Command::Create(Resource::Node {
                kind,
                pos: [0.0, 0.0],
                ws,
            }));
        };
        add(&mut s, NodeKind::Port, ws1);
        add(&mut s, NodeKind::Network, ws1);
        add(&mut s, NodeKind::Port, ws2);

        let full = s.view();
        assert_eq!(
            full.host_ports.len(),
            2,
            "both tabs' hostports in full view"
        );

        let v1 = full.for_workspace(ws1);
        assert_eq!(v1.host_ports.len(), 1, "only ws1's hostport");
        assert_eq!(v1.net_nodes.len(), 1, "only ws1's network");
        assert!(v1.node_ids.iter().all(|id| full.node_ws[id] == ws1));

        let v2 = full.for_workspace(ws2);
        assert_eq!(v2.host_ports.len(), 1);
        assert_eq!(v2.net_nodes.len(), 0, "ws2 has no network");
    }

    /// A node's TYPE is the dependency it runs; its NAME is its own, derived
    /// from its id and saying nothing about the type — so two nodes of one
    /// type are two obviously distinct things, and neither is "the python one".
    #[test]
    fn a_node_is_named_after_itself_not_its_type() {
        let dir = std::env::temp_dir().join("wk-node-naming-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wasm = dir.join("echo.wasm");
        // Not a real component: launch only needs the path to exist, and this
        // test is about naming, not about anything the guest does.
        std::fs::write(&wasm, b"\0asm\x01\0\0\0").unwrap();
        let path = dir.join("naming.wk");
        let doc = Document {
            dependencies: vec![Dependency {
                name: "python".into(),
                source: crate::workspace::Source::Path(wasm.clone()),
                args: Vec::new(),
                description: None,
            }],
            ..Document::empty()
        };
        let mut s = Server::new(&doc, path.clone()).expect("server constructs");
        let ws = s.graph.workspaces[0];
        let add = |s: &mut Server| {
            s.apply(Command::Create(Resource::Node {
                kind: NodeKind::App { dep: 0 },
                pos: [10.0, 10.0],
                ws,
            }));
        };
        add(&mut s);
        add(&mut s);
        let names: Vec<String> = s
            .node_reg
            .lock()
            .unwrap()
            .iter()
            .map(|n| n.name.clone())
            .collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1], "two nodes, two names");
        for n in &names {
            assert!(
                !n.contains("python"),
                "a name must not read as a type: {n:?}"
            );
            assert!(n.contains('-'), "the generated shape is two words: {n:?}");
        }
        // Both are the same type, and the type is the node's keyword argument.
        assert!(
            s.graph.node_deps.values().all(|d| d == "python"),
            "two nodes, one type"
        );

        // Nothing about the name is written down: it comes from the id, which
        // is already in the file, so a save adds no `name` line and a reload
        // produces exactly the same two names.
        s.save();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("node \"python\"").count(), 2, "{text}");
        assert!(!text.contains("name "), "no name needs saving:\n{text}");

        let back = Document::load(&path).expect("reloads");
        let s2 = Server::new(&back, path.clone()).expect("reconstructs");
        let mut after: Vec<String> = s2
            .node_reg
            .lock()
            .unwrap()
            .iter()
            .map(|n| n.name.clone())
            .collect();
        let mut before = names.clone();
        before.sort();
        after.sort();
        assert_eq!(after, before, "names survive a round trip unchanged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Router bridges the Networks it is wired to, and only while it is
    /// wired to at least two: a router with one wire (or none) is not a bridge,
    /// and must not leave a half-open one behind on the fabric.
    #[test]
    fn a_router_bridges_only_while_it_joins_two_networks() {
        let mut s = fresh_server();
        let ws = s.graph.workspaces[0];
        let mk = |s: &mut Server, kind| {
            let before: HashSet<NodeId> = s.graph.nodes.keys().copied().collect();
            s.apply(Command::Create(Resource::Node {
                kind,
                pos: [10.0, 10.0],
                ws,
            }));
            *s.graph
                .nodes
                .keys()
                .find(|id| !before.contains(id))
                .expect("a node was created")
        };
        let a = mk(&mut s, NodeKind::Network);
        let b = mk(&mut s, NodeKind::Network);
        let r = mk(&mut s, NodeKind::Router);
        assert!(s.view().routers.contains(&r), "the client sees a router");
        assert!(
            !s.routers.contains_key(&r),
            "an unwired router bridges nothing"
        );

        s.toggle_net(r, a);
        assert!(
            !s.routers.contains_key(&r),
            "one network is not a bridge — and a port with a single net would \
             let its members reach each other, which they already can"
        );

        // The second wire opens the bridge. Both nets are on it: unlike every
        // other member, a router's links accumulate instead of replacing.
        s.toggle_net(r, b);
        let port = s.routers.get(&r).expect("wired to two nets, so bridging");
        let mut nets = port.nets();
        nets.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(nets, want);

        // Unwiring closes it again, and so does deleting the router.
        s.toggle_net(r, b);
        assert!(
            !s.routers.contains_key(&r),
            "back to one net, back to no bridge"
        );
        s.toggle_net(r, b);
        assert!(s.routers.contains_key(&r));
        s.apply(Command::Delete(ResourceRef::Node(r)));
        assert!(
            !s.routers.contains_key(&r),
            "a deleted router takes its bridge"
        );
        assert!(
            !s.graph.net_links.iter().any(|&(m, _)| m == r),
            "and its wires"
        );
    }

    /// A Router is an ordinary canvas node in the file: it round-trips, and
    /// its several net wires all survive (the one thing that would break if it
    /// were saved like any other member, which may only be on one network).
    #[test]
    fn a_router_and_its_several_wires_round_trip() {
        let path = std::env::temp_dir().join("wk-router-roundtrip-test.wk");
        let _ = std::fs::remove_file(&path);
        let (ws, a, b, r) = (
            NodeId::from_u128(301),
            NodeId::from_u128(302),
            NodeId::from_u128(303),
            NodeId::from_u128(304),
        );
        std::fs::write(
            &path,
            format!(
                "workspace \"{ws}\" {{\n    \
                   network \"{a}\" {{ pos 0 0; size 10 10 }}\n    \
                   network \"{b}\" {{ pos 0 40; size 10 10 }}\n    \
                   router \"{r}\" {{ pos 60 20; size 10 10 }}\n    \
                   netlink \"{r}\" \"{a}\"\n    \
                   netlink \"{r}\" \"{b}\"\n}}\n"
            ),
        )
        .unwrap();
        Document::load(&path).expect("parses").save(&path).unwrap();
        let normalized = std::fs::read_to_string(&path).unwrap();

        let s = Server::new(&Document::load(&path).expect("loads"), path.clone())
            .expect("server constructs");
        assert_eq!(s.kind_of(r), Some(Kind::Router));
        assert_eq!(
            s.routers.get(&r).expect("bridging").nets().len(),
            2,
            "both wires reached the fabric"
        );
        s.save();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), normalized);
        let _ = std::fs::remove_file(&path);
    }

    /// `view().fs_providers` reports the app nodes whose compiled component
    /// serves a filesystem (imports `wk:fs/provider`) — nothing while a node is
    /// still compiling — and `for_workspace` scopes the set to its tab.
    #[test]
    fn view_reports_fs_provider_apps() {
        use crate::plugin::{Node, NodeSetup};
        let mut s = fresh_server();
        let ws1 = s.graph.workspaces[0];
        let ws2 = NodeId::new();
        s.apply(Command::Create(Resource::Workspace { id: ws2 }));

        // Two live app nodes (registry stubs — `serves_fs` reads the published
        // setup, not real wasm): one provider in ws1, one plain app in ws2.
        let stub = |id: NodeId, name: &str, fs_provider: bool| {
            let node = Arc::new(Node {
                id,
                name: name.to_string(),
                term_io: crate::terminal::TermIo::new(),
                fs: crate::vfs::new_fs(),
                midi_in: crate::midi::new_inbox(),
                options: crate::options::new_options(Vec::new()),
                finished: Arc::new(AtomicBool::new(false)),
                running: Arc::new(AtomicBool::new(false)),
                kill: Arc::new(AtomicBool::new(false)),
                setup: std::sync::OnceLock::new(),
                env: Vec::new(),
                layers: Vec::new(),
                capture_src: crate::capture::new_src(),
                clip_src: crate::clipboard::new_src(),
                clip_read: crate::clipboard::new_permit(),
                clip_write: crate::clipboard::new_permit(),
                exec_permit: crate::exec::new_permit(true),
                fs_serve: wk_vfs::ProviderConn::new(),
            });
            let _ = node.setup.set(NodeSetup {
                net_stack: None,
                http_path: None,
                run: None,
                midi: false,
                net: false,
                capture: false,
                clipboard: false,
                fs_provider,
            });
            node
        };
        let provider = NodeId::new();
        s.place(provider, Kind::App, ws1, [0.0, 0.0], [100.0, 100.0]);
        s.node_reg.lock().unwrap().push(stub(provider, "srv", true));
        let plain = NodeId::new();
        s.place(plain, Kind::App, ws2, [0.0, 0.0], [100.0, 100.0]);
        s.node_reg.lock().unwrap().push(stub(plain, "app", false));
        // A third app still compiling (no setup): never a provider.
        let compiling = NodeId::new();
        s.place(compiling, Kind::App, ws1, [0.0, 0.0], [100.0, 100.0]);

        let full = s.view();
        assert_eq!(
            full.fs_providers,
            HashSet::from([provider]),
            "only the app whose setup serves wk:fs"
        );
        assert!(full.for_workspace(ws1).fs_providers.contains(&provider));
        assert!(full.for_workspace(ws2).fs_providers.is_empty());
    }

    /// A provider wire must survive the window where its endpoint is still
    /// compiling. Two paths: a *saved* app→app connection loads as a
    /// connection (the .wk file already names the relation — classifying it
    /// at load mistyped it as MIDI, the filesystems.wk regression), and an
    /// *interactive* app↔app wire made during the window defers in
    /// `pending_app_wires` until both setups publish, then classifies right.
    #[test]
    fn provider_wires_survive_a_compiling_provider() {
        use crate::plugin::{Node, NodeSetup};
        let mut s = fresh_server();
        let ws = s.graph.workspaces[0];
        let stub = |id: NodeId, name: &str| {
            Arc::new(Node {
                id,
                name: name.to_string(),
                term_io: crate::terminal::TermIo::new(),
                fs: crate::vfs::new_fs(),
                midi_in: crate::midi::new_inbox(),
                options: crate::options::new_options(Vec::new()),
                finished: Arc::new(AtomicBool::new(false)),
                running: Arc::new(AtomicBool::new(false)),
                kill: Arc::new(AtomicBool::new(false)),
                setup: std::sync::OnceLock::new(),
                env: Vec::new(),
                layers: Vec::new(),
                capture_src: crate::capture::new_src(),
                clip_src: crate::clipboard::new_src(),
                clip_read: crate::clipboard::new_permit(),
                clip_write: crate::clipboard::new_permit(),
                exec_permit: crate::exec::new_permit(true),
                fs_serve: wk_vfs::ProviderConn::new(),
            })
        };
        let published = |node: &Arc<Node>, fs_provider: bool| {
            let _ = node.setup.set(NodeSetup {
                net_stack: None,
                http_path: None,
                run: None,
                midi: false,
                net: false,
                capture: false,
                clipboard: false,
                fs_provider,
            });
        };

        // Both apps exist but are still compiling (no setup published).
        let provider = NodeId::new();
        let consumer = NodeId::new();
        s.place(provider, Kind::App, ws, [0.0, 0.0], [100.0, 100.0]);
        s.place(consumer, Kind::App, ws, [0.0, 0.0], [100.0, 100.0]);
        let pnode = stub(provider, "zipfs");
        let cnode = stub(consumer, "bash");
        s.node_reg.lock().unwrap().push(pnode.clone());
        s.node_reg.lock().unwrap().push(cnode.clone());

        // Load path: the saved relation is applied as itself, compiling or not.
        let mut saved = crate::workspace::Workspace::new();
        saved.id = ws;
        saved.connections.push((provider, consumer));
        s.instantiate(&saved);
        assert!(
            s.graph.connections.contains(&(provider, consumer)),
            "a saved connection stays a connection"
        );
        assert!(s.graph.midi_links.is_empty(), "never mistyped as MIDI");

        // Interactive path: a wire drawn during the window waits, then
        // classifies as a mount once the provider's setup publishes.
        s.graph.connections.clear();
        s.pending_app_wires.clear();
        s.connect_toggle(provider, consumer);
        assert!(s.graph.connections.is_empty() && s.graph.midi_links.is_empty());
        assert_eq!(s.pending_app_wires.len(), 1, "deferred while compiling");
        published(&pnode, true);
        s.sync_pending_app_wires();
        assert_eq!(s.pending_app_wires.len(), 1, "waits for BOTH endpoints");
        published(&cnode, false);
        s.sync_pending_app_wires();
        assert!(
            s.graph.connections.contains(&(provider, consumer)),
            "classified as a provider mount once decidable"
        );
        assert!(s.graph.midi_links.is_empty());
    }

    /// Dropping a file from the OS creates a BindMount already pointed at
    /// the path (Resource::HostMount), named by its basename, in one
    /// undoable step.
    #[test]
    fn host_mount_creates_a_pointed_bind_in_one_step() {
        let mut s = fresh_server();
        let ws = s.graph.workspaces[0];
        s.apply(Command::Create(Resource::HostMount {
            path: "/some/where/data.csv".into(),
            pos: [10.0, 20.0],
            ws,
        }));
        let (&id, bind) = s
            .graph
            .file_nodes
            .iter()
            .find_map(|(id, f)| match f {
                FileNode::Bind(b) => Some((id, b)),
                _ => None,
            })
            .expect("a bind node exists");
        assert_eq!(bind.name, "data.csv", "named by basename");
        assert_eq!(bind.path, PathBuf::from("/some/where/data.csv"));
        assert!(s.graph.nodes.contains_key(&id), "placed on the canvas");

        // One undo removes it — the create was recorded as a single step.
        s.apply(Command::Undo);
        assert!(s.graph.file_nodes.is_empty(), "undo uncreates the mount");
    }

    /// A HostPort's localhost port can be set absolutely via `port_set` (what
    /// `wk create port <n>` / `wk node set --port` use).
    #[test]
    fn set_host_port_sets_the_port_absolutely() {
        let mut s = fresh_server();
        let ws = s.graph.workspaces[0];
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Port,
            pos: [0.0, 0.0],
            ws,
        }));
        let id = *s.graph.host_ports.keys().next().expect("a hostport");
        s.apply(Command::Update {
            id,
            patch: NodePatch {
                port_set: Some(3000),
                ..Default::default()
            },
        });
        assert_eq!(s.graph.host_ports.get(&id).copied(), Some(3000));
    }

    /// `SetServePort` records a serve wire's container port in the graph and
    /// `serve_port_for` reports it; `0` resets to the HostPort's own port.
    #[test]
    fn set_serve_port_maps_then_resets() {
        let mut s = fresh_server();
        let served = NodeId::new();
        let hostport = NodeId::new();
        s.graph.serve_links.push((served, hostport));
        // Default: forward verbatim (the host port).
        assert_eq!(s.serve_port_for(served, hostport, 8080), 8080);

        s.apply(Command::SetServePort {
            served,
            hostport,
            container: 3000,
        });
        assert_eq!(
            s.graph.serve_ports.get(&(served, hostport)).copied(),
            Some(3000)
        );
        assert_eq!(s.serve_port_for(served, hostport, 8080), 3000);

        s.apply(Command::SetServePort {
            served,
            hostport,
            container: 0,
        });
        assert!(!s.graph.serve_ports.contains_key(&(served, hostport)));
        assert_eq!(s.serve_port_for(served, hostport, 8080), 8080);
    }

    /// `SetMount` overrides where a bind mounts and remembers it in the graph;
    /// an empty path resets to the default (the volume's name at the root).
    #[test]
    fn set_mount_overrides_then_resets_to_default() {
        let mut s = fresh_server();
        let ws = s.graph.workspaces[0];
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Volume,
            pos: [0.0, 0.0],
            ws,
        }));
        let vol = *s
            .graph
            .file_nodes
            .keys()
            .next()
            .expect("a volume was placed");
        let default = s.graph.file_nodes[&vol].name().to_string();
        // A bind is (volume, app); a stand-in app id is enough for the path model.
        let app = NodeId::new();
        s.graph.connections.push((vol, app));
        assert_eq!(s.mount_path_for(vol, app), default, "default is the name");

        s.apply(Command::SetMount {
            volume: vol,
            app,
            path: "/data/notes.txt".into(),
        });
        assert_eq!(
            s.graph.mount_paths.get(&(vol, app)).map(String::as_str),
            Some("/data/notes.txt")
        );
        assert_eq!(s.mount_path_for(vol, app), "/data/notes.txt");

        // Blank path clears the override; the default returns.
        s.apply(Command::SetMount {
            volume: vol,
            app,
            path: "   ".into(),
        });
        assert!(!s.graph.mount_paths.contains_key(&(vol, app)));
        assert_eq!(s.mount_path_for(vol, app), default);
    }

    /// `--host-path` (a BindMount patch) repoints the node at a host path and
    /// derives its default mount name from the new basename.
    #[test]
    fn set_bind_path_repoints_a_bindmount() {
        let mut s = fresh_server();
        let ws = s.graph.workspaces[0];
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::BindMount,
            pos: [0.0, 0.0],
            ws,
        }));
        let id = *s.graph.file_nodes.keys().next().expect("a bind mount");
        s.apply(Command::Update {
            id,
            patch: NodePatch {
                host_path: Some("/srv/data/logs".into()),
                ..Default::default()
            },
        });
        match s.graph.file_nodes.get(&id) {
            Some(FileNode::Bind(f)) => {
                assert_eq!(f.path, PathBuf::from("/srv/data/logs"));
                assert_eq!(f.name, "logs", "mount name follows the new basename");
            }
            _ => panic!("expected a BindMount node"),
        }
    }

    /// A persisted volume's bytes survive a save→reload (written to a sidecar
    /// beside the `.wk`); an ephemeral one comes back empty.
    #[test]
    fn persisted_volume_bytes_survive_reload() {
        let path = std::env::temp_dir().join("wk-vol-persist-test.wk");
        let _ = std::fs::remove_file(&path);
        let mut sidecar = path.clone().into_os_string();
        sidecar.push(".volumes");
        let _ = std::fs::remove_dir_all(PathBuf::from(&sidecar));

        let vol = {
            let mut s = Server::new(&Document::empty(), path.clone()).expect("server");
            let ws = s.graph.workspaces[0];
            s.apply(Command::Create(Resource::Node {
                kind: NodeKind::Volume,
                pos: [0.0, 0.0],
                ws,
            }));
            let vol = *s.graph.file_nodes.keys().next().expect("a volume");
            s.apply(Command::Update {
                id: vol,
                patch: NodePatch {
                    persist: Some(true),
                    ..Default::default()
                },
            });
            if let Some(FileNode::Volume(v)) = s.graph.file_nodes.get(&vol) {
                v.data.lock().unwrap().extend_from_slice(b"remember me");
            }
            s.save();
            vol
        };

        // Reload from disk: the sidecar restores the bytes.
        let doc = crate::workspace::Document::load_resolved(&path).expect("reload");
        let s2 = Server::new(&doc, path.clone()).expect("server");
        match s2.graph.file_nodes.get(&vol) {
            Some(FileNode::Volume(v)) => {
                assert!(v.persist, "persist flag round-trips");
                assert_eq!(&*v.data.lock().unwrap(), b"remember me");
            }
            _ => panic!("expected the volume to reload"),
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(PathBuf::from(&sidecar));
    }

    /// Turning off a volume's persistence removes its sidecar on the next save,
    /// so stale bytes don't linger.
    /// Hiding a node's 3D panel is cosmetic arrangement, and it outlives the
    /// session: the flag reaches the view, the file, and the reloaded server.
    #[test]
    fn hiding_a_3d_panel_persists_across_a_reload() {
        let path = std::env::temp_dir().join("wk-panel3d-test.wk");
        let _ = std::fs::remove_file(&path);

        let mut s = Server::new(&Document::empty(), path.clone()).expect("server");
        let ws = s.graph.workspaces[0];
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Volume,
            pos: [0.0, 0.0],
            ws,
        }));
        let id = *s.graph.file_nodes.keys().next().expect("a node");

        let hide = Command::Update {
            id,
            patch: NodePatch {
                panel3d: Some(false),
                ..Default::default()
            },
        };
        // Showing or hiding a panel is layout, not reconfiguration — a
        // client with only `Arrange` may do it.
        assert_eq!(
            hide.required(),
            (
                wk_protocol::ResourceKind::Node,
                wk_protocol::Action::Arrange
            )
        );
        s.apply(hide);
        assert!(s.graph.hidden_panel3d.contains(&id));
        assert!(s.view().hidden_panel3d.contains(&id));

        s.save();
        let text = std::fs::read_to_string(&path).expect("saved");
        assert!(text.contains("panel3d #false"), "not in the file: {text}");
        let doc = Document::load(&path).expect("re-parses");
        let reloaded = Server::new(&doc, path.clone()).expect("server");
        assert!(
            reloaded.graph.hidden_panel3d.contains(&id),
            "the reloaded node lost its hidden panel"
        );

        // Showing it again clears the flag, and the file stops mentioning it.
        let mut s = reloaded;
        s.apply(Command::Update {
            id,
            patch: NodePatch {
                panel3d: Some(true),
                ..Default::default()
            },
        });
        assert!(!s.graph.hidden_panel3d.contains(&id));
        s.save();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("panel3d"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unpersisting_a_volume_prunes_its_sidecar() {
        let path = std::env::temp_dir().join("wk-vol-prune-test.wk");
        let _ = std::fs::remove_file(&path);
        let mut sidecar_dir = path.clone().into_os_string();
        sidecar_dir.push(".volumes");
        let sidecar_dir = PathBuf::from(&sidecar_dir);
        let _ = std::fs::remove_dir_all(&sidecar_dir);

        let mut s = Server::new(&Document::empty(), path.clone()).expect("server");
        let ws = s.graph.workspaces[0];
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Volume,
            pos: [0.0, 0.0],
            ws,
        }));
        let vol = *s.graph.file_nodes.keys().next().expect("a volume");
        s.apply(Command::Update {
            id: vol,
            patch: NodePatch {
                persist: Some(true),
                ..Default::default()
            },
        });
        s.save();
        assert!(
            s.volume_sidecar(vol).exists(),
            "persisted → sidecar written"
        );

        // Turn persistence back off; the sidecar is pruned on the next save.
        s.apply(Command::Update {
            id: vol,
            patch: NodePatch {
                persist: Some(false),
                ..Default::default()
            },
        });
        s.save();
        assert!(
            !s.volume_sidecar(vol).exists(),
            "unpersisted → sidecar removed"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&sidecar_dir);
    }

    /// A bind's mount path survives a save→reload cycle (it persists as the
    /// connection's 3rd KDL arg).
    #[test]
    fn mount_path_survives_save_and_reload() {
        let path = std::env::temp_dir().join("wk-mount-persist-test.wk");
        let _ = std::fs::remove_file(&path);
        let mut s = Server::new(&Document::empty(), path.clone()).expect("server");
        let ws = s.graph.workspaces[0];
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Volume,
            pos: [0.0, 0.0],
            ws,
        }));
        let vol = *s.graph.file_nodes.keys().next().expect("a volume");
        let app = NodeId::new();
        s.graph.connections.push((vol, app));
        s.apply(Command::SetMount {
            volume: vol,
            app,
            path: "/data/notes.txt".into(),
        });
        assert_eq!(
            s.graph.mount_paths.get(&(vol, app)).map(String::as_str),
            Some("/data/notes.txt"),
            "set in the live graph"
        );
        s.save();
        let doc = crate::workspace::Document::load_resolved(&path).expect("reload");
        let saved = doc.workspaces.iter().find(|w| w.id == ws).expect("ws");
        assert_eq!(
            saved.mount_paths.get(&(vol, app)).map(String::as_str),
            Some("/data/notes.txt"),
            "persisted as the connection's mount path"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `wk view` is a request to whoever is *looking*, carried in the view as
    /// (sequence, mode): each request advances the sequence exactly once, so a
    /// client applies it once and a client attaching later isn't yanked by an
    /// old one. It touches nothing else — least of all the document.
    #[test]
    fn a_view_request_advances_a_sequence_and_changes_nothing_else() {
        let mut s = fresh_server();
        let before = s.view();
        assert_eq!(before.view_mode.0, 0, "nothing asked yet");

        s.apply(Command::SetView(ViewMode::World));
        let after = s.view();
        assert_eq!(after.view_mode, (1, ViewMode::World));
        assert_eq!(after.node_ids, before.node_ids, "no document change");

        // A second request advances again, so a client that already applied
        // the first still sees this one — even to the same mode.
        s.apply(Command::SetView(ViewMode::World));
        assert_eq!(s.view().view_mode, (2, ViewMode::World));

        // And it is not undoable: undo has nothing to pop.
        s.apply(Command::Undo);
        assert_eq!(s.view().view_mode.0, 2);
    }

    /// A node whose dependency isn't in the list (renamed/removed, or an
    /// offline uplink) can't materialize — but load→save must not silently
    /// delete it or the wire touching it. It round-trips verbatim so re-adding
    /// the dependency brings it back, wired.
    #[test]
    fn unresolvable_node_and_its_wire_survive_save() {
        use crate::workspace::{NodeSnap, SnapKind};
        let ws = NodeId::new();
        let ghost = NodeId::new();
        let file = NodeId::new();
        let doc = Document {
            imports: Vec::new(),
            imported_deps: std::collections::HashSet::new(),
            imported_workspaces: std::collections::HashSet::new(),
            scratch_tab: None,
            dependencies: Vec::new(), // "ghost" isn't here
            workspaces: vec![Workspace {
                id: ws,
                name: None,
                tab: true,
                nodes: vec![
                    NodeSnap {
                        id: ghost,
                        pos: [10.0, 10.0],
                        size: [360.0, 260.0],
                        pos3d: None,
                        panel3d: true,
                        kind: SnapKind::App {
                            dep: "ghost".into(),
                            name: None,
                            options: vec![1.0, 2.0],
                            args: vec!["hello".into()],
                            token: None,
                        },
                    },
                    NodeSnap {
                        id: file,
                        pos: [20.0, 20.0],
                        size: [130.0, 44.0],
                        pos3d: None,
                        panel3d: true,
                        kind: SnapKind::Volume {
                            name: "file1".into(),
                            persist: false,
                        },
                    },
                ],
                connections: vec![(file, ghost)],
                mount_paths: std::collections::BTreeMap::new(),
                midi: Vec::new(),
                serves: Vec::new(),
                serve_ports: std::collections::BTreeMap::new(),
                capture_links: Vec::new(),
                clipboard_links: Vec::new(),
                api_links: Vec::new(),
                net_links: Vec::new(),
            }],
        };
        let path = std::env::temp_dir().join("wk-unresolvable-test.wk");
        let server = Server::new(&doc, path.clone()).expect("server constructs");
        // The file placed; the ghost didn't (unknown dep) but is remembered.
        assert!(server.node_exists(file));
        assert!(!server.node_exists(ghost));
        server.save();

        let back = Document::load(&path).expect("reloads");
        let w = &back.workspaces[0];
        let ghost_snap = w
            .nodes
            .iter()
            .find(|n| n.id == ghost)
            .expect("ghost node preserved, not deleted");
        assert_eq!(
            ghost_snap.kind,
            SnapKind::App {
                dep: "ghost".into(),
                name: None,
                options: vec![1.0, 2.0],
                args: vec!["hello".into()],
                token: None,
            }
        );
        assert!(
            w.connections.contains(&(file, ghost)),
            "the wire to the unresolved node is preserved"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A workspace's name is authored content the Server has to *carry*: `save`
    /// is a full re-projection from the live graph, so anything the runtime
    /// doesn't model is erased from the `.wk` file on the first clean exit.
    /// Load → run → save → load must give the name back, and both client paths
    /// (the local UI's `View`, the CLI's `Snapshot`) must see it.
    #[test]
    fn workspace_names_survive_a_load_and_save_round_trip() {
        let named = NodeId::new();
        let unnamed = NodeId::new();
        let doc = Document {
            imports: Vec::new(),
            imported_deps: std::collections::HashSet::new(),
            imported_workspaces: std::collections::HashSet::new(),
            scratch_tab: None,
            dependencies: Vec::new(),
            workspaces: vec![
                Workspace {
                    id: named,
                    name: Some("voice".into()),
                    ..Workspace::new()
                },
                Workspace {
                    id: unnamed,
                    ..Workspace::new()
                },
            ],
        };
        let path = std::env::temp_dir().join("wk-workspace-name-test.wk");
        let _ = std::fs::remove_file(&path);
        let mut s = Server::new(&doc, path.clone()).expect("server constructs");
        assert_eq!(
            s.graph.workspace_names.get(&named).map(String::as_str),
            Some("voice")
        );
        assert!(!s.graph.workspace_names.contains_key(&unnamed));

        // The tab bar draws *every* tab, so narrowing the view to one workspace
        // must not strip the other tabs' names.
        let full = s.view();
        assert_eq!(
            full.for_workspace(unnamed)
                .workspace_names
                .get(&named)
                .map(String::as_str),
            Some("voice")
        );
        assert_eq!(
            s.ipc_snapshot()
                .workspace_names
                .get(&named)
                .map(String::as_str),
            Some("voice")
        );

        s.save();
        let back = Document::load(&path).expect("reloads");
        let find = |id| back.workspaces.iter().find(|w| w.id == id).expect("tab");
        assert_eq!(find(named).name.as_deref(), Some("voice"));
        assert_eq!(find(unnamed).name, None, "an unnamed tab stays unnamed");
        let _ = std::fs::remove_file(&path);
    }

    /// A file that is nothing but definitions still needs somewhere to stand,
    /// so the loader invents a tab — but that tab is the loader's, not the
    /// author's. Writing it back grew a stray `workspace "…" { }` block in the
    /// user's library file on every single run.
    #[test]
    fn the_invented_tab_of_a_definitions_only_file_is_not_written_back() {
        let path = std::env::temp_dir().join("wk-scratch-tab-test.wk");
        let _ = std::fs::remove_file(&path);
        let def = NodeId::from_u128(201);
        let original = format!("workspace \"{def}\" {{\n    name \"voice\"\n    tab #false\n}}\n");
        std::fs::write(&path, &original).unwrap();
        Document::load(&path).expect("parses").save(&path).unwrap();
        let normalized = std::fs::read_to_string(&path).unwrap();

        let doc = Document::load_resolved(&path).expect("loads");
        let scratch = doc.scratch_tab.expect("a definitions-only file gets a tab");
        let s = Server::new(&doc, path.clone()).expect("server constructs");
        assert_eq!(
            s.graph.workspaces,
            vec![scratch],
            "there is a tab to stand on"
        );

        s.save();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            normalized,
            "running a library file must not write to it"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The other half: the invented tab is skipped because it is *empty*, not
    /// because it is invented. Put something in it and it is a workspace like
    /// any other — otherwise a node dropped on it would vanish at exit.
    #[test]
    fn the_invented_tab_is_saved_once_something_is_put_in_it() {
        let path = std::env::temp_dir().join("wk-scratch-tab-used-test.wk");
        let _ = std::fs::remove_file(&path);
        let def = NodeId::from_u128(202);
        std::fs::write(
            &path,
            format!("workspace \"{def}\" {{\n    name \"voice\"\n    tab #false\n}}\n"),
        )
        .unwrap();

        let doc = Document::load_resolved(&path).expect("loads");
        let scratch = doc.scratch_tab.expect("a definitions-only file gets a tab");
        let mut s = Server::new(&doc, path.clone()).expect("server constructs");
        s.apply(Command::Create(Resource::Node {
            kind: NodeKind::Network,
            pos: [10.0, 10.0],
            ws: scratch,
        }));
        s.save();

        let back = Document::load(&path).expect("reloads");
        let tab = back
            .workspaces
            .iter()
            .find(|w| w.id == scratch)
            .expect("the tab is in the file now");
        assert_eq!(tab.nodes.len(), 1, "with what was put on it");
        let _ = std::fs::remove_file(&path);
    }

    /// `save` is a full re-projection from the live graph, so a workspace the
    /// runtime deliberately does *not* run — a `tab #false` definition — would
    /// be written out of existence on the first clean exit. The Server has to
    /// carry the authored block instead: load → run → save must hand the file
    /// back unchanged, comments and block order included.
    #[test]
    fn a_definition_workspace_survives_a_load_and_save_cycle_unchanged() {
        let path = std::env::temp_dir().join("wk-definition-authored-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, def) = (NodeId::from_u128(101), NodeId::from_u128(102));
        let (hp, vol, synth) = (
            NodeId::from_u128(103),
            NodeId::from_u128(104),
            NodeId::from_u128(105),
        );
        // A running tab, then a definition holding everything the runtime would
        // otherwise have to model to reproduce: a name, nodes, a wire, and a
        // mount-path override.
        let original = format!(
            "workspace \"{tab}\" {{\n    \
               hostport \"{hp}\" {{ port 8080; pos 0 0; size 10 10 }}\n}}\n\
             // the voice, used from elsewhere\n\
             workspace \"{def}\" {{\n    \
               name \"voice\"\n    \
               tab #false\n    \
               volume \"chan\" \"{vol}\" {{ persist #true; pos 1 2; size 30 40 }}\n    \
               node \"synth\" \"{synth}\" {{ pos 5 6; size 70 80; options 8 0.5 }}\n    \
               connection \"{vol}\" \"{synth}\" \"/mnt/chan\"\n}}\n"
        );
        // Normalize once through the file writer, so the comparison below is
        // about content rather than whitespace the formatter would rewrite on
        // any save at all.
        std::fs::write(&path, &original).unwrap();
        Document::load(&path).expect("parses").save(&path).unwrap();
        let normalized = std::fs::read_to_string(&path).unwrap();
        assert!(
            normalized.contains("tab #false") && normalized.contains("// the voice"),
            "the fixture itself has to carry what must survive:\n{normalized}"
        );

        let doc = Document::load(&path).expect("loads");
        let mut s = Server::new(&doc, path.clone()).expect("server constructs");
        // The definition does not run and no client hears about it: none of its
        // nodes exist, and it is not a tab.
        assert_eq!(s.graph.workspaces, vec![tab]);
        assert_eq!(s.ipc_snapshot().workspaces, vec![tab]);
        assert!(!s.node_exists(vol) && !s.node_exists(synth));
        assert!(
            s.graph.workspace_names.is_empty(),
            "a definition's name is not a tab name"
        );
        // ...and not being modelled did not make it an *unplaced* node either:
        // the whole block is held one level up.
        assert!(s.unplaced.is_empty());

        s.save();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            normalized,
            "the definition did not come back byte-for-byte"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A volume inside a `tab #false` definition is in the file but not live,
    /// so it has no bytes to re-save. Pruning sidecars by what is *running*
    /// would delete that volume's data on the way out, silently and for good.
    #[test]
    fn a_volume_in_a_definition_keeps_its_sidecar() {
        let path = std::env::temp_dir().join("wk-definition-volume-test.wk");
        let _ = std::fs::remove_file(&path);
        let (tab, def) = (NodeId::from_u128(111), NodeId::from_u128(112));
        let vol = NodeId::from_u128(113);
        let doc = Document {
            imports: Vec::new(),
            imported_deps: std::collections::HashSet::new(),
            imported_workspaces: std::collections::HashSet::new(),
            scratch_tab: None,
            dependencies: Vec::new(),
            workspaces: vec![
                Workspace {
                    id: tab,
                    ..Workspace::new()
                },
                Workspace {
                    id: def,
                    tab: false,
                    nodes: vec![NodeSnap {
                        id: vol,
                        pos: [0.0, 0.0],
                        size: [10.0, 10.0],
                        pos3d: None,
                        panel3d: true,
                        kind: SnapKind::Volume {
                            name: "chan".into(),
                            persist: true,
                        },
                    }],
                    ..Workspace::new()
                },
            ],
        };
        let s = Server::new(&doc, path.clone()).expect("server constructs");
        let _ = std::fs::remove_dir_all(s.volume_dir());
        std::fs::create_dir_all(s.volume_dir()).unwrap();
        // Bytes written by an earlier run, back when the definition was a tab.
        std::fs::write(s.volume_sidecar(vol), b"remember me").unwrap();
        // And a sidecar for a volume nothing in the file mentions any more.
        let stale = s.volume_dir().join(NodeId::from_u128(114).to_string());
        std::fs::write(&stale, b"nobody's").unwrap();

        s.save();
        assert_eq!(
            std::fs::read(s.volume_sidecar(vol)).unwrap(),
            b"remember me",
            "a volume the file still asks to persist lost its bytes"
        );
        assert!(
            !stale.exists(),
            "a sidecar no volume claims is still pruned"
        );
        let _ = std::fs::remove_dir_all(s.volume_dir());
        let _ = std::fs::remove_file(&path);
    }

    /// Closing a tab takes its name with it — a name outliving its workspace
    /// would resurface on whatever tab was created next — and undoing the close
    /// brings the name back, not just the (then anonymous) tab.
    #[test]
    fn closing_a_workspace_drops_its_name_and_undo_restores_it() {
        let mut s = fresh_server();
        let ws2 = NodeId::new();
        s.apply(Command::Create(Resource::Workspace { id: ws2 }));
        s.graph.workspace_names.insert(ws2, "voice".into());

        s.apply(Command::Delete(ResourceRef::Workspace(ws2)));
        assert!(!s.graph.workspaces.contains(&ws2));
        assert!(!s.graph.workspace_names.contains_key(&ws2));

        s.apply(Command::Undo);
        assert!(s.graph.workspaces.contains(&ws2));
        assert_eq!(
            s.graph.workspace_names.get(&ws2).map(String::as_str),
            Some("voice"),
            "undo restored the tab but forgot what it was called"
        );
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
        let ticket = a.view().uplinks[&ia].ticket.clone();

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
            let (pa, pb) = (a.view().uplinks[&ia].peers, b.view().uplinks[&ib].peers);
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

    /// Like `iroh_nodes_wire_and_dial_between_servers`, but over Veilid: two
    /// servers each grow a Veilid node wired to a Network; pasting one's ticket
    /// (a DHT record key) into the other establishes routed peers both ways.
    /// Needs the public Veilid network (bootstrap, DHT, private routes), and
    /// attaching can take tens of seconds — ignored by default, run manually:
    /// `cargo test -p wk-server veilid_nodes -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the public Veilid network; slow attach"]
    fn veilid_nodes_wire_and_dial_between_servers() {
        let mut a = fresh_server();
        let mut b = fresh_server();
        let setup = |s: &mut Server| {
            let ws = s.graph.workspaces[0];
            s.apply(Command::Create(Resource::Node {
                kind: NodeKind::Veilid,
                pos: [0.0, 0.0],
                ws,
            }));
            s.apply(Command::Create(Resource::Node {
                kind: NodeKind::Network,
                pos: [100.0, 0.0],
                ws,
            }));
            let uplink = *s.graph.veilid_ids.keys().next().expect("veilid node");
            let net = s
                .graph
                .nodes
                .iter()
                .find(|(_, r)| r.kind == Kind::Network)
                .map(|(&id, _)| id)
                .expect("network node");
            s.apply(Command::Create(Resource::Wire { a: uplink, b: net }));
            uplink
        };
        let va = setup(&mut a);
        let vb = setup(&mut b);
        let ticket = a.view().uplinks[&va].ticket.clone();
        eprintln!("[test] dialing {ticket}");

        b.apply(Command::Update {
            id: vb,
            patch: NodePatch {
                args: Some(ticket),
                ..Default::default()
            },
        });

        // Attach + DHT publish + route import can take a while on the real
        // network; the dialer retries every 5s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            let (pa, pb) = (a.view().uplinks[&va].peers, b.view().uplinks[&vb].peers);
            if pa >= 1 && pb >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "veilid uplinks never connected (peers: a={pa} b={pb})"
            );
            std::thread::sleep(std::time::Duration::from_secs(2));
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
            Op::CreateFile => s.apply(create(NodeKind::Volume)),
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
                    pos3d: None,
                    panel3d: Some(false),
                    pos: Some([1.0, 2.0]),
                    size: Some([3.0, 4.0]),
                    args: Some("ghost".into()),
                    port_delta: None,
                    port_set: None,
                    text: None,
                    host_path: None,
                    midi_device: None,
                    persist: None,
                    service_name: None,
                    service_target: None,
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
        for id in &s.graph.hidden_panel3d {
            prop_assert!(base.contains(id), "orphan hidden_panel3d entry");
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
