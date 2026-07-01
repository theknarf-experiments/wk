//! The wk **workspace file**: a `.wk` file (KDL syntax; `workspace.wk` by
//! default) holding a project's shared *dependencies* plus one or more
//! *workspaces* (canvas tabs), each with its own id, nodes, and wiring.
//!
//! ```kdl
//! dependencies {
//!     triangle "plugins/triangle/.../triangle.wasm"
//!     foo      "oci://ghcr.io/org/foo:1.0"
//! }
//! workspace "0000000000000000000000000M" {
//!     node "synth" "0000000000000000000000000N" { pos 19 88; size 360 260 }
//!     midi "0000000000000000000000000N" "0000000000000000000000000P"
//! }
//! ```

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use std::path::{Path, PathBuf};
use wk_protocol::NodeId;

/// The default workspace file when none is named on the command line.
pub const DEFAULT_WORKSPACE: &str = "workspace.wk";

/// Written as the first line of every `.wk` file so editors highlight it as KDL
/// despite the custom extension. `//` is a KDL comment, so it round-trips
/// harmlessly (the parser ignores it).
const MODELINE: &str = "// vim: set filetype=kdl :";

/// Where a dependency's wasm comes from.
#[derive(Debug, Clone)]
pub enum Source {
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

    /// Pull + cache an OCI artifact if it isn't already cached. A no-op for local paths.
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
    /// Args passed to the plugin (after argv[0] = name); e.g. a filename.
    pub args: Vec<String>,
}

impl Dependency {
    pub fn local_path(&self) -> PathBuf {
        self.source.local_path()
    }

    pub fn ensure(&self) -> Result<(), String> {
        self.source.ensure()
    }
}

/// A placed node: an app instance (`node`) or a file node (`virtualfile`/`hostfile`).
#[derive(Clone)]
pub struct NodeState {
    /// Dependency name (for app nodes) or file name (for file nodes).
    pub name: String,
    pub id: NodeId,
    pub pos: [f32; 2],
    pub size: [f32; 2],
    /// App-node option values (knob settings), persisted positionally.
    pub options: Vec<f32>,
    /// App-node launch args, editable in the GUI.
    pub args: Vec<String>,
}

/// A HostPort node: a localhost port plus its canvas placement.
#[derive(Clone)]
pub struct PortState {
    pub id: NodeId,
    pub port: u16,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

/// A Network (or Gateway) node and its canvas placement.
#[derive(Clone)]
pub struct NetState {
    pub id: NodeId,
    pub gateway: bool,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

/// A `.wk` file: shared dependencies plus one or more workspaces (canvas tabs).
#[derive(Clone)]
pub struct Document {
    pub dependencies: Vec<Dependency>,
    /// Always at least one; shown as tabs when there is more than one.
    pub workspaces: Vec<Workspace>,
}

/// One workspace: a canvas of nodes and the wiring between them, with its own id.
#[derive(Clone)]
pub struct Workspace {
    pub id: NodeId,
    pub nodes: Vec<NodeState>,
    pub virtual_files: Vec<NodeState>,
    pub host_files: Vec<NodeState>,
    pub host_ports: Vec<PortState>,
    pub connections: Vec<(NodeId, NodeId)>,
    pub midi: Vec<(NodeId, NodeId)>,
    pub serves: Vec<(NodeId, NodeId)>,
    pub nets: Vec<NetState>,
    pub net_links: Vec<(NodeId, NodeId)>,
}

impl Workspace {
    /// A fresh, empty workspace with a new id.
    pub fn new() -> Self {
        Workspace {
            id: NodeId::new(),
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
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    /// An empty document: no dependencies, one blank workspace.
    pub fn empty() -> Self {
        Document {
            dependencies: Vec::new(),
            workspaces: vec![Workspace::new()],
        }
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "no {} in this directory ({e}); create one with `wk init`",
                path.display()
            )
        })?;
        Self::from_kdl(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

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

        let mut workspaces: Vec<Workspace> = doc
            .nodes()
            .iter()
            .filter(|n| n.name().value() == "workspace")
            .filter_map(parse_workspace)
            .collect();
        if workspaces.is_empty() {
            workspaces.push(Workspace::new());
        }

        Ok(Document {
            dependencies,
            workspaces,
        })
    }

    fn to_kdl(&self) -> String {
        let mut doc = KdlDocument::new();

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

        for ws in &self.workspaces {
            doc.nodes_mut().push(workspace_kdl(ws));
        }

        doc.autoformat();
        // Lead with a modeline so `.wk` files highlight as KDL in editors.
        format!("{MODELINE}\n{doc}")
    }
}

fn num(v: &KdlValue) -> Option<f32> {
    v.as_float()
        .map(|f| f as f32)
        .or_else(|| v.as_integer().map(|i| i as f32))
}

fn uint(v: &KdlValue) -> Option<u64> {
    v.as_integer().map(|i| i as u64)
}

/// Parse a node id from its Crockford base32 string form.
fn node_id(v: &KdlValue) -> Option<NodeId> {
    v.as_string()?.parse().ok()
}

/// Parse a `workspace "<id>" { ...canvas... }` block.
fn parse_workspace(n: &KdlNode) -> Option<Workspace> {
    let id = node_id(n.get(0)?)?;
    let pair = |n: &KdlNode| match (n.get(0).and_then(node_id), n.get(1).and_then(node_id)) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    };
    let mut ws = Workspace {
        id,
        ..Workspace::new()
    };
    for c in n.children().map(|ch| ch.nodes()).unwrap_or(&[]) {
        match c.name().value() {
            "node" => ws.nodes.extend(parse_placed(c)),
            "virtualfile" => ws.virtual_files.extend(parse_placed(c)),
            "hostfile" => ws.host_files.extend(parse_placed(c)),
            "hostport" => ws.host_ports.extend(parse_hostport(c)),
            "connection" => ws.connections.extend(pair(c)),
            "midi" => ws.midi.extend(pair(c)),
            "serve" => ws.serves.extend(pair(c)),
            "network" => ws.nets.extend(parse_net(c, false)),
            "gateway" => ws.nets.extend(parse_net(c, true)),
            "netlink" => ws.net_links.extend(pair(c)),
            _ => {}
        }
    }
    Some(ws)
}

fn workspace_kdl(ws: &Workspace) -> KdlNode {
    let mut node = KdlNode::new("workspace");
    node.push(KdlEntry::new(ws.id.to_string()));
    let mut ch = KdlDocument::new();
    for n in &ws.nodes {
        ch.nodes_mut().push(placed_kdl("node", n));
    }
    for f in &ws.virtual_files {
        ch.nodes_mut().push(placed_kdl("virtualfile", f));
    }
    for f in &ws.host_files {
        ch.nodes_mut().push(placed_kdl("hostfile", f));
    }
    for hp in &ws.host_ports {
        ch.nodes_mut().push(hostport_kdl(hp));
    }
    for &(file, node) in &ws.connections {
        ch.nodes_mut().push(pair_kdl("connection", file, node));
    }
    for &(src, dst) in &ws.midi {
        ch.nodes_mut().push(pair_kdl("midi", src, dst));
    }
    for &(http, hostport) in &ws.serves {
        ch.nodes_mut().push(pair_kdl("serve", http, hostport));
    }
    for n in &ws.nets {
        ch.nodes_mut().push(net_kdl(n));
    }
    for &(app, net) in &ws.net_links {
        ch.nodes_mut().push(pair_kdl("netlink", app, net));
    }
    node.set_children(ch);
    node
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

/// A KDL node `name "<id>" "<id>"` joining two nodes.
fn pair_kdl(name: &str, a: NodeId, b: NodeId) -> KdlNode {
    let mut n = KdlNode::new(name);
    n.push(KdlEntry::new(a.to_string()));
    n.push(KdlEntry::new(b.to_string()));
    n
}

/// Create a new empty workspace file at `path`. Errors if one exists.
pub fn init(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    Document::empty().save(path)?;
    println!("created {}", path.display());
    Ok(())
}

/// Add a plugin to the file as a dependency. `target` is a local `.wasm` path or
/// an `oci://<ref>` registry reference; the name is its file stem or the OCI
/// repository's last segment. An OCI artifact is pulled now to validate it.
pub fn add(target: String, path: &Path) -> Result<(), String> {
    let mut doc = Document::load(path)?;
    let source = Source::parse(&target);
    let name = match &source {
        Source::Path(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".to_string()),
        Source::Oci(reference) => crate::oci::name_for(reference),
    };
    source.ensure()?;
    if doc.dependencies.iter().any(|d| d.name == name) {
        println!("dependency already present: {name}");
        return Ok(());
    }
    doc.dependencies.push(Dependency {
        name: name.clone(),
        source,
        args: Vec::new(),
    });
    doc.save(path)?;
    println!("added dependency: {name}");
    Ok(())
}

/// Publish a local plugin to an OCI registry as a Wasm OCI Artifact. `plugin` is
/// a dependency name (resolved to its local wasm) or a `.wasm` path; `reference`
/// is the target, e.g. `localhost:5000/triangle:1.0`.
pub fn publish(plugin: String, reference: String, path: &Path) -> Result<(), String> {
    let wasm = Document::load(path)
        .ok()
        .and_then(|d| d.dependencies.into_iter().find(|d| d.name == plugin))
        .map(|d| d.local_path())
        .unwrap_or_else(|| PathBuf::from(&plugin));
    let bytes = std::fs::read(&wasm).map_err(|e| format!("reading {}: {e}", wasm.display()))?;
    crate::oci::push(&reference, &bytes)?;
    println!("published {} -> oci://{reference}", wasm.display());
    Ok(())
}

/// Print the file's dependencies.
pub fn list(path: &Path) -> Result<(), String> {
    let doc = Document::load(path)?;
    if doc.dependencies.is_empty() {
        println!("(no dependencies; add one with `wk add <path>`)");
    }
    for dep in &doc.dependencies {
        println!("  {}  {}", dep.name, dep.source.to_kdl());
    }
    Ok(())
}

/// Remove a dependency by name.
pub fn remove(name: String, path: &Path) -> Result<(), String> {
    let mut doc = Document::load(path)?;
    let before = doc.dependencies.len();
    doc.dependencies.retain(|d| d.name != name);
    match before - doc.dependencies.len() {
        0 => println!("no dependency named {name:?}"),
        n => {
            doc.save(path)?;
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
    fn document_kdl_round_trips() {
        let (wa, wb, synth, chan, msrc, mdst, port, notes, net, gw) = (
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
            NodeId::new(),
        );
        let doc = Document {
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
            workspaces: vec![
                Workspace {
                    id: wa,
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
                },
                Workspace {
                    id: wb,
                    ..Workspace::new()
                },
            ],
        };

        let text = doc.to_kdl();
        assert!(text.starts_with(MODELINE), "starts with the modeline");
        let back = Document::from_kdl(&text).expect("parses (modeline ignored)");
        assert_eq!(back.dependencies.len(), 2);
        assert_eq!(back.dependencies[0].name, "triangle");
        assert_eq!(back.dependencies[1].args, vec!["example.com", "80"]);
        assert!(matches!(back.dependencies[1].source, Source::Oci(_)));

        assert_eq!(back.workspaces.len(), 2);
        let a = &back.workspaces[0];
        assert_eq!(a.id, wa);
        assert_eq!(a.nodes.len(), 1);
        assert_eq!(a.nodes[0].name, "synth");
        assert_eq!(a.nodes[0].options, vec![8.0, 0.6, 0.0, 1.0]);
        assert_eq!(a.nodes[0].args, vec!["netserve".to_string(), "80".into()]);
        assert!(a.virtual_files[0].options.is_empty());
        assert_eq!(a.host_files[0].name, "notes.txt");
        assert_eq!(a.host_ports[0].port, 8080);
        assert_eq!(a.connections, vec![(chan, synth)]);
        assert_eq!(a.midi, vec![(msrc, mdst)]);
        assert_eq!(a.serves, vec![(synth, port)]);
        assert_eq!(a.nets.len(), 2);
        assert!(a.nets[1].gateway);
        assert_eq!(a.net_links, vec![(synth, net)]);

        assert_eq!(back.workspaces[1].id, wb);
        assert!(back.workspaces[1].nodes.is_empty());
    }
}
