//! The wk **workspace**: a `.wk` file (KDL syntax; `workspace.wk` by default,
//! but several can share a directory) that holds everything about a project —
//! both its *manifest* (the dependencies it can launch) and its *session* (the
//! live canvas: camera, the nodes that were open and where, and the connections
//! wiring them). One file, one type, one reader and one writer — a `wk run`
//! reopens exactly where you left off, and `wk add`/`wk remove` edit
//! dependencies without disturbing the layout (it all round-trips through
//! [`Workspace`]).
//!
//! ```kdl
//! dependencies {
//!     triangle "plugins/triangle/.../triangle.wasm"
//!     foo      "oci://ghcr.io/org/foo:1.0"
//! }
//! camera { pan 0 0; zoom 1 }
//! node "synth" 1 { pos 19 88; size 360 260; options 0.3 2.0 0.0 1800.0 }
//! virtualfile "chan" 2 { pos 400 120; size 130 44 }
//! hostfile "notes.txt" 6 { pos 400 200; size 130 44 }
//! connection 2 1
//! midi 3 4
//! hostport 5 { port 8080; pos 600 100; size 130 44 }
//! serve 1 5
//! network 7 { pos 700 100; size 130 44 }
//! gateway 8 { pos 700 200; size 130 44 }
//! netlink 1 7
//! ```

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use std::path::{Path, PathBuf};
use wk_protocol::NodeId;

/// The default workspace file when none is named on the command line. Several
/// `.wk` workspaces can coexist in one directory.
pub const DEFAULT_WORKSPACE: &str = "workspace.wk";

/// Written as the first line of every `.wk` file so editors highlight it as KDL
/// despite the custom extension. `//` is a KDL comment, so it round-trips
/// harmlessly (the parser ignores it).
const MODELINE: &str = "// vim: set filetype=kdl :";

// ---- manifest: dependencies ----

/// Where a dependency's wasm comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// A local `.wasm` file.
    Path(PathBuf),
    /// An OCI registry reference (e.g. `ghcr.io/org/name:1.0`), pulled + cached.
    Oci(String),
}

impl Source {
    /// Parse the string form stored in the workspace file (an `oci://` prefix means OCI).
    pub fn parse(s: &str) -> Self {
        match s.strip_prefix("oci://") {
            Some(reference) => Source::Oci(reference.to_string()),
            None => Source::Path(PathBuf::from(s)),
        }
    }

    /// The string written back to the workspace file.
    pub fn to_kdl(&self) -> String {
        match self {
            Source::Path(p) => p.to_string_lossy().into_owned(),
            Source::Oci(reference) => format!("oci://{reference}"),
        }
    }

    /// The local path to load the wasm from. For OCI this is the cache location
    /// (which [`Source::ensure`] populates); it may not exist until then.
    pub fn local_path(&self) -> PathBuf {
        match self {
            Source::Path(p) => p.clone(),
            Source::Oci(reference) => crate::oci::cache_path(reference),
        }
    }

    /// Make the wasm available locally: pull + cache an OCI artifact if it isn't
    /// already cached. A no-op for local paths.
    pub fn ensure(&self) -> Result<(), String> {
        if let Source::Oci(reference) = self {
            let path = crate::oci::cache_path(reference);
            if !path.exists() {
                println!("pulling {reference} ...");
                let bytes = crate::oci::pull(reference)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

/// One workspace dependency: a short name resolving to a plugin source.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub name: String,
    pub source: Source,
    /// Command-line arguments passed to the plugin (after argv[0] = name), e.g.
    /// a filename for an editor. Set in the workspace file as `name "path" { args "..." }`.
    pub args: Vec<String>,
}

impl Dependency {
    /// The local path to load this dependency's wasm from.
    pub fn local_path(&self) -> PathBuf {
        self.source.local_path()
    }

    /// Pull + cache the dependency if it's an OCI artifact not yet local.
    pub fn ensure(&self) -> Result<(), String> {
        self.source.ensure()
    }
}

// ---- session: placed nodes on the canvas ----

/// A placed node: an app instance (`node`) or a file node (`virtualfile`/
/// `hostfile`).
pub struct NodeState {
    /// Dependency name (for app nodes) or file name (for file nodes).
    pub name: String,
    pub id: NodeId,
    pub pos: [f32; 2],
    pub size: [f32; 2],
    /// App-node option values (e.g. knob settings), persisted positionally.
    /// Empty for file nodes (and app nodes that report none).
    pub options: Vec<f32>,
    /// App-node launch args (e.g. a client's target host/port), editable in the
    /// GUI. Empty for file nodes and nodes left at their dependency default.
    pub args: Vec<String>,
}

/// A HostPort node: a localhost port plus its canvas placement.
pub struct PortState {
    pub id: NodeId,
    pub port: u16,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

/// A Network (or Gateway) node and its canvas placement.
pub struct NetState {
    pub id: NodeId,
    pub gateway: bool,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

/// The whole workspace: the manifest (dependencies) and the session (the canvas).
pub struct Workspace {
    pub dependencies: Vec<Dependency>,
    /// Canvas camera: pan x, pan y, zoom.
    pub camera: (f32, f32, f32),
    pub nodes: Vec<NodeState>,
    /// In-memory VirtualFile nodes; `name` holds the mount name.
    pub virtual_files: Vec<NodeState>,
    /// HostMappedFile nodes; `name` holds the host file path.
    pub host_files: Vec<NodeState>,
    /// HostPort nodes (localhost port + canvas placement).
    pub host_ports: Vec<PortState>,
    /// File connections as (file id, app node id).
    pub connections: Vec<(NodeId, NodeId)>,
    /// MIDI connections as (source node id, destination node id).
    pub midi: Vec<(NodeId, NodeId)>,
    /// Serve wiring as (wasi:http node id, HostPort id).
    pub serves: Vec<(NodeId, NodeId)>,
    /// Network/Gateway nodes.
    pub nets: Vec<NetState>,
    /// Network membership wiring as (app node id, Network/Gateway node id).
    pub net_links: Vec<(NodeId, NodeId)>,
}

impl Workspace {
    /// An empty workspace (no dependencies, blank canvas).
    pub fn empty() -> Self {
        Workspace {
            dependencies: Vec::new(),
            camera: (0.0, 0.0, 1.0),
            nodes: Vec::new(),
            virtual_files: Vec::new(),
            host_files: Vec::new(),
            host_ports: Vec::new(),
            connections: Vec::new(),
            midi: Vec::new(),
            serves: Vec::new(),
            nets: Vec::new(),
            net_links: Vec::new(),
        }
    }

    /// Load a workspace from the given `.wk` file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "no {} in this directory ({e}); create one with `wk init`",
                path.display()
            )
        })?;
        Self::from_kdl(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Write the whole workspace to the given `.wk` file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, self.to_kdl())
            .map_err(|e| format!("failed to write {}: {e}", path.display()))
    }

    fn from_kdl(text: &str) -> Result<Self, String> {
        let doc: KdlDocument = text.parse().map_err(|e| format!("parse error: {e}"))?;

        let dependencies = doc
            .get("dependencies")
            .and_then(|n| n.children())
            .map(|ch| {
                ch.nodes()
                    .iter()
                    .filter_map(|n| {
                        // Tolerate an npm-style trailing colon on the name.
                        let name = n.name().value().trim_end_matches(':').to_string();
                        let source = n.get(0).and_then(|v| v.as_string())?;
                        let args = n
                            .children()
                            .and_then(|ch| ch.get("args"))
                            .map(|a| {
                                a.entries()
                                    .iter()
                                    .filter_map(|e| e.value().as_string().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some(Dependency {
                            name,
                            source: Source::parse(source),
                            args,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut camera = (0.0, 0.0, 1.0);
        if let Some(cam) = doc.get("camera").and_then(|n| n.children()) {
            if let Some(pan) = cam.get("pan") {
                if let (Some(x), Some(y)) = (pan.get(0).and_then(num), pan.get(1).and_then(num)) {
                    camera.0 = x;
                    camera.1 = y;
                }
            }
            if let Some(z) = cam.get("zoom").and_then(|n| n.get(0)).and_then(num) {
                camera.2 = z;
            }
        }

        let pair = |n: &KdlNode| match (n.get(0).and_then(node_id), n.get(1).and_then(node_id)) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        };

        let mut nodes = Vec::new();
        let mut virtual_files = Vec::new();
        let mut host_files = Vec::new();
        let mut host_ports = Vec::new();
        let mut connections = Vec::new();
        let mut midi = Vec::new();
        let mut serves = Vec::new();
        let mut nets = Vec::new();
        let mut net_links = Vec::new();
        for n in doc.nodes() {
            match n.name().value() {
                "node" => nodes.extend(parse_placed(n)),
                "virtualfile" => virtual_files.extend(parse_placed(n)),
                "hostfile" => host_files.extend(parse_placed(n)),
                "hostport" => host_ports.extend(parse_hostport(n)),
                "connection" => connections.extend(pair(n)),
                "midi" => midi.extend(pair(n)),
                "serve" => serves.extend(pair(n)),
                "network" => nets.extend(parse_net(n, false)),
                "gateway" => nets.extend(parse_net(n, true)),
                "netlink" => net_links.extend(pair(n)),
                _ => {} // dependencies / unknown
            }
        }

        Ok(Workspace {
            dependencies,
            camera,
            nodes,
            virtual_files,
            host_files,
            host_ports,
            connections,
            midi,
            serves,
            nets,
            net_links,
        })
    }

    fn to_kdl(&self) -> String {
        let mut doc = KdlDocument::new();

        // Manifest.
        let mut deps = KdlNode::new("dependencies");
        let mut children = KdlDocument::new();
        for dep in &self.dependencies {
            let mut node = KdlNode::new(dep.name.clone());
            node.push(KdlEntry::new(dep.source.to_kdl()));
            if !dep.args.is_empty() {
                let mut sub = KdlDocument::new();
                let mut args_node = KdlNode::new("args");
                for a in &dep.args {
                    args_node.push(KdlEntry::new(a.clone()));
                }
                sub.nodes_mut().push(args_node);
                node.set_children(sub);
            }
            children.nodes_mut().push(node);
        }
        deps.set_children(children);
        doc.nodes_mut().push(deps);

        // Session.
        let mut cam = KdlNode::new("camera");
        let mut cam_ch = KdlDocument::new();
        cam_ch
            .nodes_mut()
            .push(node2("pan", self.camera.0, self.camera.1));
        let mut zoom = KdlNode::new("zoom");
        zoom.push(KdlEntry::new(self.camera.2 as f64));
        cam_ch.nodes_mut().push(zoom);
        cam.set_children(cam_ch);
        doc.nodes_mut().push(cam);

        for n in &self.nodes {
            doc.nodes_mut().push(placed_kdl("node", n));
        }
        for f in &self.virtual_files {
            doc.nodes_mut().push(placed_kdl("virtualfile", f));
        }
        for f in &self.host_files {
            doc.nodes_mut().push(placed_kdl("hostfile", f));
        }
        for hp in &self.host_ports {
            doc.nodes_mut().push(hostport_kdl(hp));
        }
        for &(file, node) in &self.connections {
            doc.nodes_mut().push(pair_kdl("connection", file, node));
        }
        for &(src, dst) in &self.midi {
            doc.nodes_mut().push(pair_kdl("midi", src, dst));
        }
        for &(http, hostport) in &self.serves {
            doc.nodes_mut().push(pair_kdl("serve", http, hostport));
        }
        for n in &self.nets {
            doc.nodes_mut().push(net_kdl(n));
        }
        for &(app, net) in &self.net_links {
            doc.nodes_mut().push(pair_kdl("netlink", app, net));
        }

        doc.autoformat();
        // Lead with a modeline so `.wk` files highlight as KDL in editors.
        format!("{MODELINE}\n{doc}")
    }
}

// ---- KDL parse/write helpers ----

fn num(v: &KdlValue) -> Option<f32> {
    v.as_float()
        .map(|f| f as f32)
        .or_else(|| v.as_integer().map(|i| i as f32))
}

fn uint(v: &KdlValue) -> Option<u64> {
    v.as_integer().map(|i| i as u64)
}

/// Parse a node id: the Crockford base32 string form, or — for backward
/// compatibility with pre-UUID workspaces — a bare integer, mapped 1:1 so old
/// ids (and the connections that reference them) still load, then re-save as
/// base32.
fn node_id(v: &KdlValue) -> Option<NodeId> {
    if let Some(s) = v.as_string() {
        s.parse().ok()
    } else {
        v.as_integer().map(|i| NodeId::from_u128(i as u128))
    }
}

/// Parse a `node`/`virtualfile`/`hostfile` entry: `<kind> "<name>" <id> { ... }`.
fn parse_placed(n: &KdlNode) -> Option<NodeState> {
    let name = n.get(0)?.as_string()?.to_string();
    let id = node_id(n.get(1)?)?;
    let ch = n.children()?;
    let pos = ch.get("pos")?;
    let size = ch.get("size")?;
    let options = ch
        .get("options")
        .map(|o| o.entries().iter().filter_map(|e| num(e.value())).collect())
        .unwrap_or_default();
    let args = ch
        .get("args")
        .map(|a| {
            a.entries()
                .iter()
                .filter_map(|e| e.value().as_string().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(NodeState {
        name,
        id,
        pos: [pos.get(0).and_then(num)?, pos.get(1).and_then(num)?],
        size: [size.get(0).and_then(num)?, size.get(1).and_then(num)?],
        options,
        args,
    })
}

/// Parse a `hostport <id> { port <p>; pos x y; size w h }` entry.
fn parse_hostport(n: &KdlNode) -> Option<PortState> {
    let id = node_id(n.get(0)?)?;
    let ch = n.children()?;
    let port = ch.get("port").and_then(|p| p.get(0)).and_then(uint)? as u16;
    let pos = ch.get("pos")?;
    let size = ch.get("size")?;
    Some(PortState {
        id,
        port,
        pos: [pos.get(0).and_then(num)?, pos.get(1).and_then(num)?],
        size: [size.get(0).and_then(num)?, size.get(1).and_then(num)?],
    })
}

fn hostport_kdl(p: &PortState) -> KdlNode {
    let mut node = KdlNode::new("hostport");
    node.push(KdlEntry::new(p.id.to_string()));
    let mut ch = KdlDocument::new();
    let mut port = KdlNode::new("port");
    port.push(KdlEntry::new(p.port as i128));
    ch.nodes_mut().push(port);
    ch.nodes_mut().push(node2("pos", p.pos[0], p.pos[1]));
    ch.nodes_mut().push(node2("size", p.size[0], p.size[1]));
    node.set_children(ch);
    node
}

/// Parse a `network`/`gateway <id> { pos x y; size w h }` entry.
fn parse_net(n: &KdlNode, gateway: bool) -> Option<NetState> {
    let id = node_id(n.get(0)?)?;
    let ch = n.children()?;
    let pos = ch.get("pos")?;
    let size = ch.get("size")?;
    Some(NetState {
        id,
        gateway,
        pos: [pos.get(0).and_then(num)?, pos.get(1).and_then(num)?],
        size: [size.get(0).and_then(num)?, size.get(1).and_then(num)?],
    })
}

fn net_kdl(n: &NetState) -> KdlNode {
    let mut node = KdlNode::new(if n.gateway { "gateway" } else { "network" });
    node.push(KdlEntry::new(n.id.to_string()));
    let mut ch = KdlDocument::new();
    ch.nodes_mut().push(node2("pos", n.pos[0], n.pos[1]));
    ch.nodes_mut().push(node2("size", n.size[0], n.size[1]));
    node.set_children(ch);
    node
}

fn placed_kdl(kind: &str, n: &NodeState) -> KdlNode {
    let mut node = KdlNode::new(kind);
    node.push(KdlEntry::new(n.name.clone()));
    node.push(KdlEntry::new(n.id.to_string()));
    let mut ch = KdlDocument::new();
    ch.nodes_mut().push(node2("pos", n.pos[0], n.pos[1]));
    ch.nodes_mut().push(node2("size", n.size[0], n.size[1]));
    if !n.options.is_empty() {
        let mut opts = KdlNode::new("options");
        for &v in &n.options {
            opts.push(KdlEntry::new(v as f64));
        }
        ch.nodes_mut().push(opts);
    }
    if !n.args.is_empty() {
        let mut args = KdlNode::new("args");
        for a in &n.args {
            args.push(KdlEntry::new(a.clone()));
        }
        ch.nodes_mut().push(args);
    }
    node.set_children(ch);
    node
}

/// A KDL node `name a b` with two float args.
fn node2(name: &str, a: f32, b: f32) -> KdlNode {
    let mut n = KdlNode::new(name);
    n.push(KdlEntry::new(a as f64));
    n.push(KdlEntry::new(b as f64));
    n
}

/// A KDL node `name a b` with two integer-id args.
fn pair_kdl(name: &str, a: NodeId, b: NodeId) -> KdlNode {
    let mut n = KdlNode::new(name);
    n.push(KdlEntry::new(a.to_string()));
    n.push(KdlEntry::new(b.to_string()));
    n
}

// ---- CLI commands (each operates on the given workspace file) ----

/// Create a new empty workspace at `path`. Errors if one exists.
pub fn init(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    Workspace::empty().save(path)?;
    println!("created {}", path.display());
    Ok(())
}

/// Add a plugin to the workspace as a dependency. `target` is a local `.wasm`
/// path or an `oci://<ref>` registry reference; the name is its file stem or the
/// OCI repository's last segment. An OCI artifact is pulled now to validate it.
pub fn add(target: String, path: &Path) -> Result<(), String> {
    let mut ws = Workspace::load(path)?;
    let source = Source::parse(&target);
    let name = match &source {
        Source::Path(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".to_string()),
        Source::Oci(reference) => crate::oci::name_for(reference),
    };
    source.ensure()?;
    if ws.dependencies.iter().any(|d| d.name == name) {
        println!("dependency already in workspace: {name}");
        return Ok(());
    }
    ws.dependencies.push(Dependency {
        name: name.clone(),
        source,
        args: Vec::new(),
    });
    ws.save(path)?;
    println!("added dependency: {name}");
    Ok(())
}

/// Publish a local plugin to an OCI registry as a Wasm OCI Artifact. `plugin` is
/// a dependency name (resolved to its local wasm) or a `.wasm` path; `reference`
/// is the target, e.g. `localhost:5000/triangle:1.0`.
pub fn publish(plugin: String, reference: String, path: &Path) -> Result<(), String> {
    let wasm = Workspace::load(path)
        .ok()
        .and_then(|w| w.dependencies.into_iter().find(|d| d.name == plugin))
        .map(|d| d.local_path())
        .unwrap_or_else(|| PathBuf::from(&plugin));
    let bytes = std::fs::read(&wasm).map_err(|e| format!("reading {}: {e}", wasm.display()))?;
    crate::oci::push(&reference, &bytes)?;
    println!("published {} -> oci://{reference}", wasm.display());
    Ok(())
}

/// Print the workspace's dependencies.
pub fn list(path: &Path) -> Result<(), String> {
    let ws = Workspace::load(path)?;
    if ws.dependencies.is_empty() {
        println!("(no dependencies; add one with `wk add <path>`)");
    }
    for dep in &ws.dependencies {
        println!("  {}  {}", dep.name, dep.source.to_kdl());
    }
    Ok(())
}

/// Remove a dependency from the workspace by name.
pub fn remove(name: String, path: &Path) -> Result<(), String> {
    let mut ws = Workspace::load(path)?;
    let before = ws.dependencies.len();
    ws.dependencies.retain(|d| d.name != name);
    match before - ws.dependencies.len() {
        0 => println!("no dependency named {name:?}"),
        n => {
            ws.save(path)?;
            println!("removed {n} dependency named {name:?}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_parse_and_roundtrip() {
        match Source::parse("oci://ghcr.io/org/foo:1.0") {
            Source::Oci(r) => assert_eq!(r, "ghcr.io/org/foo:1.0"),
            other => panic!("expected oci, got {other:?}"),
        }
        assert!(matches!(Source::parse("plugins/x.wasm"), Source::Path(_)));
        assert_eq!(
            Source::Oci("ghcr.io/o/f:1".into()).to_kdl(),
            "oci://ghcr.io/o/f:1"
        );
        assert_eq!(Source::Path("a/b.wasm".into()).to_kdl(), "a/b.wasm");
    }

    #[test]
    fn workspace_kdl_round_trips() {
        let (synth, chan, msrc, mdst, port, notes, net, gw) = (
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
        );
        let ws = Workspace {
            dependencies: vec![
                Dependency {
                    name: "triangle".into(),
                    source: Source::Path("plugins/triangle.wasm".into()),
                    args: Vec::new(),
                },
                Dependency {
                    name: "fetch".into(),
                    source: Source::Oci("ghcr.io/o/fetch:1".into()),
                    args: vec!["example.com".into(), "80".into()],
                },
            ],
            camera: (12.5, -40.0, 1.5),
            nodes: vec![NodeState {
                name: "synth".into(),
                id: synth,
                pos: [40.0, 56.0],
                size: [360.0, 260.0],
                options: vec![8.0, 0.6, 0.0, 1.0],
                args: vec!["netserve".into(), "80".into()],
            }],
            virtual_files: vec![NodeState {
                name: "chan".into(),
                id: chan,
                pos: [200.0, 120.0],
                size: [130.0, 44.0],
                options: Vec::new(),
                args: Vec::new(),
            }],
            host_files: vec![NodeState {
                name: "notes.txt".into(),
                id: notes,
                pos: [200.0, 200.0],
                size: [130.0, 44.0],
                options: Vec::new(),
                args: Vec::new(),
            }],
            host_ports: vec![PortState {
                id: port,
                port: 8080,
                pos: [600.0, 100.0],
                size: [130.0, 44.0],
            }],
            connections: vec![(chan, synth)],
            midi: vec![(msrc, mdst)],
            serves: vec![(synth, port)],
            nets: vec![
                NetState {
                    id: net,
                    gateway: false,
                    pos: [700.0, 100.0],
                    size: [130.0, 44.0],
                },
                NetState {
                    id: gw,
                    gateway: true,
                    pos: [700.0, 200.0],
                    size: [130.0, 44.0],
                },
            ],
            net_links: vec![(synth, net)],
        };

        let text = ws.to_kdl();
        // First line is the editor modeline; it must not break parsing.
        assert!(text.starts_with(MODELINE), "starts with the modeline");
        let back = Workspace::from_kdl(&text).expect("parses (modeline ignored)");
        // Manifest.
        assert_eq!(back.dependencies.len(), 2);
        assert_eq!(back.dependencies[0].name, "triangle");
        assert_eq!(back.dependencies[1].args, vec!["example.com", "80"]);
        assert!(matches!(back.dependencies[1].source, Source::Oci(_)));
        // Session.
        assert_eq!(back.camera, ws.camera);
        assert_eq!(back.nodes.len(), 1);
        assert_eq!(back.nodes[0].name, "synth");
        assert_eq!(back.nodes[0].options, vec![8.0, 0.6, 0.0, 1.0]);
        assert_eq!(
            back.nodes[0].args,
            vec!["netserve".to_string(), "80".into()]
        );
        assert!(back.virtual_files[0].options.is_empty());
        assert_eq!(back.host_files[0].name, "notes.txt");
        assert_eq!(back.host_ports[0].port, 8080);
        assert_eq!(back.connections, vec![(chan, synth)]);
        assert_eq!(back.midi, vec![(msrc, mdst)]);
        assert_eq!(back.serves, vec![(synth, port)]);
        assert_eq!(back.nets.len(), 2);
        assert!(back.nets[1].gateway);
        assert_eq!(back.net_links, vec![(synth, net)]);
    }
}
