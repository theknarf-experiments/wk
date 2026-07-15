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

use kdl::{KdlDocument, KdlEntry, KdlEntryFormat, KdlNode, KdlValue};
use std::path::{Path, PathBuf};
use wk_protocol::NodeId;

/// A KDL entry for a string value that always serializes *quoted*.
///
/// Works around a kdl-6 formatter/parser asymmetry: the formatter emits certain
/// strings (e.g. `-.0`, or a bare `.`) as unquoted identifiers that its own
/// parser then rejects — so a user-supplied node arg or name like that would
/// make the whole saved `.wk` file fail to load. Others (number- or
/// keyword-shaped) would parse as a non-string and be silently dropped. Forcing
/// an explicit quoted representation keeps every string value round-trippable.
fn str_entry(s: &str) -> KdlEntry {
    let mut e = KdlEntry::new(s.to_string());
    e.set_format(KdlEntryFormat {
        value_repr: kdl_quote(s),
        leading: " ".to_string(),
        // Keep `value_repr` through `KdlDocument::autoformat`.
        autoformat_keep: true,
        ..Default::default()
    });
    e
}

/// Escape a string into a KDL quoted-string literal.
fn kdl_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The default workspace file when none is named on the command line.
pub const DEFAULT_WORKSPACE: &str = "workspace.wk";

/// Written as the first line of every `.wk` file so editors highlight it as KDL
/// despite the custom extension. `//` is a KDL comment, so it round-trips
/// harmlessly (the parser ignores it).
const MODELINE: &str = "// vim: set filetype=kdl :";

/// Where a dependency's wasm comes from.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct Dependency {
    pub name: String,
    pub source: Source,
    /// Args passed to the plugin (after argv[0] = name); e.g. a filename.
    pub args: Vec<String>,
    /// An optional one-line description, shown in the command palette.
    pub description: Option<String>,
}

impl Dependency {
    pub fn local_path(&self) -> PathBuf {
        self.source.local_path()
    }

    pub fn ensure(&self) -> Result<(), String> {
        self.source.ensure()
    }
}

/// One placed canvas node, of any kind: the shared unit of the `.wk` file,
/// load-time restore, and the server's undo snapshots — a node saved, deleted
/// + undone, or loaded is materialized from exactly this.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeSnap {
    pub id: NodeId,
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub kind: SnapKind,
}

/// The kind-specific part of a [`NodeSnap`]. Each variant serializes under its
/// own KDL node name (`node`, `virtualfile`, `hostfile`, `hostport`,
/// `network`/`gateway`, `iroh`, `veilid`).
#[derive(Clone, Debug, PartialEq)]
pub enum SnapKind {
    /// An app node; `name` resolves against the dependency list.
    App {
        name: String,
        /// Option values (knob settings), persisted positionally.
        options: Vec<f32>,
        /// Launch args, editable in the GUI. Empty falls back to the
        /// dependency's default args at materialization.
        args: Vec<String>,
    },
    /// An in-memory shared file. Its *bytes* are runtime state: undo carries
    /// them alongside the snap, the `.wk` file does not persist them.
    VirtualFile { name: String },
    /// A disk-backed file node (its mount name derives from the path).
    HostFile { path: PathBuf },
    /// A localhost HostPort.
    Port { port: u16 },
    /// A Network node (or Gateway — a Network granting host access).
    Net { gateway: bool },
    /// An uplink node extending a Network to a remote fabric. `secret` is the
    /// persisted identity — Iroh: a hex ed25519 key; Veilid: a DHT owner
    /// keypair string. It keeps the ticket stable across restarts, and anyone
    /// holding it can impersonate the uplink — treat the `.wk` file
    /// accordingly. `peer` is the remote ticket, re-dialed at load.
    Iroh {
        secret: Option<String>,
        peer: Option<String>,
    },
    /// See [`SnapKind::Iroh`].
    Veilid {
        secret: Option<String>,
        peer: Option<String>,
    },
}

/// Hex-encode an uplink secret for persistence.
pub fn secret_hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode a persisted uplink secret, if well-formed.
pub fn secret_bytes(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A `.wk` file: shared dependencies plus one or more workspaces (canvas tabs).
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    pub dependencies: Vec<Dependency>,
    /// Always at least one; shown as tabs when there is more than one.
    pub workspaces: Vec<Workspace>,
}

/// One workspace: a canvas of nodes and the wiring between them, with its own id.
#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub id: NodeId,
    /// Every placed node, of any kind (each serializes under its kind's KDL
    /// node name). File order is preserved.
    pub nodes: Vec<NodeSnap>,
    /// File mounts as (file id, app node id).
    pub connections: Vec<(NodeId, NodeId)>,
    /// MIDI links as (source, destination).
    pub midi: Vec<(NodeId, NodeId)>,
    /// Serve wiring as (served node id, HostPort id).
    pub serves: Vec<(NodeId, NodeId)>,
    /// Network membership as (member id, Network id).
    pub net_links: Vec<(NodeId, NodeId)>,
}

impl Workspace {
    /// A fresh, empty workspace with a new id.
    pub fn new() -> Self {
        Workspace {
            id: NodeId::new(),
            nodes: Vec::new(),
            connections: Vec::new(),
            midi: Vec::new(),
            serves: Vec::new(),
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
                        let description = n
                            .children()
                            .and_then(|ch| ch.get("description"))
                            .and_then(|d| d.get(0))
                            .and_then(|v| v.as_string())
                            .map(str::to_string);
                        Some(Dependency {
                            name,
                            source: Source::parse(source),
                            args,
                            description,
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
            node.push(str_entry(&dep.source.to_kdl()));
            let mut sub = KdlDocument::new();
            if let Some(d) = &dep.description {
                let mut desc_node = KdlNode::new("description");
                desc_node.push(str_entry(d));
                sub.nodes_mut().push(desc_node);
            }
            if !dep.args.is_empty() {
                let mut args_node = KdlNode::new("args");
                for a in &dep.args {
                    args_node.push(str_entry(a));
                }
                sub.nodes_mut().push(args_node);
            }
            if !sub.nodes().is_empty() {
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
            "connection" => ws.connections.extend(pair(c)),
            "midi" => ws.midi.extend(pair(c)),
            "serve" => ws.serves.extend(pair(c)),
            "netlink" => ws.net_links.extend(pair(c)),
            _ => ws.nodes.extend(parse_snap(c)),
        }
    }
    Some(ws)
}

fn workspace_kdl(ws: &Workspace) -> KdlNode {
    let mut node = KdlNode::new("workspace");
    node.push(KdlEntry::new(ws.id.to_string()));
    let mut ch = KdlDocument::new();
    for n in &ws.nodes {
        ch.nodes_mut().push(snap_kdl(n));
    }
    for &(file, node) in &ws.connections {
        ch.nodes_mut().push(pair_kdl("connection", file, node));
    }
    for &(src, dst) in &ws.midi {
        ch.nodes_mut().push(pair_kdl("midi", src, dst));
    }
    for &(served, hostport) in &ws.serves {
        ch.nodes_mut().push(pair_kdl("serve", served, hostport));
    }
    for &(member, net) in &ws.net_links {
        ch.nodes_mut().push(pair_kdl("netlink", member, net));
    }
    node.set_children(ch);
    node
}

/// Parse one placed node of any kind, dispatching on the KDL node name.
/// Unknown names yield `None` (tolerated, like any unknown entry).
fn parse_snap(n: &KdlNode) -> Option<NodeSnap> {
    // Named kinds (`node "<name>" <id>`) carry the name first; the rest are
    // `<kind> <id>`.
    let named = matches!(n.name().value(), "node" | "virtualfile" | "hostfile");
    let id = node_id(n.get(if named { 1 } else { 0 })?)?;
    let ch = n.children()?;
    let text = |name: &str| {
        ch.get(name)
            .and_then(|x| x.get(0))
            .and_then(|v| v.as_string())
            .map(str::to_string)
    };
    let kind = match n.name().value() {
        "node" => {
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
            SnapKind::App {
                name: n.get(0)?.as_string()?.to_string(),
                options,
                args,
            }
        }
        "virtualfile" => SnapKind::VirtualFile {
            name: n.get(0)?.as_string()?.to_string(),
        },
        "hostfile" => SnapKind::HostFile {
            path: PathBuf::from(n.get(0)?.as_string()?),
        },
        // Reject an out-of-range port (drop the node) rather than truncate it:
        // a hand-edited `port 99999` should not silently become 34463.
        "hostport" => SnapKind::Port {
            port: ch
                .get("port")
                .and_then(|p| p.get(0))
                .and_then(uint)
                .and_then(|n| u16::try_from(n).ok())?,
        },
        "network" => SnapKind::Net { gateway: false },
        "gateway" => SnapKind::Net { gateway: true },
        "iroh" => SnapKind::Iroh {
            secret: text("secret"),
            peer: text("peer"),
        },
        "veilid" => SnapKind::Veilid {
            secret: text("secret"),
            peer: text("peer"),
        },
        _ => return None,
    };
    let pos = ch.get("pos")?;
    let size = ch.get("size")?;
    Some(NodeSnap {
        id,
        pos: [pos.get(0).and_then(num)?, pos.get(1).and_then(num)?],
        size: [size.get(0).and_then(num)?, size.get(1).and_then(num)?],
        kind,
    })
}

/// Serialize one placed node under its kind's KDL node name.
fn snap_kdl(s: &NodeSnap) -> KdlNode {
    let name = match &s.kind {
        SnapKind::App { .. } => "node",
        SnapKind::VirtualFile { .. } => "virtualfile",
        SnapKind::HostFile { .. } => "hostfile",
        SnapKind::Port { .. } => "hostport",
        SnapKind::Net { gateway: false } => "network",
        SnapKind::Net { gateway: true } => "gateway",
        SnapKind::Iroh { .. } => "iroh",
        SnapKind::Veilid { .. } => "veilid",
    };
    let mut node = KdlNode::new(name);
    // Named kinds lead with the name, then the id.
    match &s.kind {
        SnapKind::App { name, .. } | SnapKind::VirtualFile { name } => {
            node.push(str_entry(name));
        }
        SnapKind::HostFile { path } => {
            node.push(str_entry(&path.to_string_lossy()));
        }
        _ => {}
    }
    node.push(KdlEntry::new(s.id.to_string()));

    let mut ch = KdlDocument::new();
    let mut child_str = |key: &str, value: &str| {
        let mut n = KdlNode::new(key);
        n.push(str_entry(value));
        ch.nodes_mut().push(n);
    };
    match &s.kind {
        SnapKind::Iroh { secret, peer } | SnapKind::Veilid { secret, peer } => {
            if let Some(sec) = secret {
                child_str("secret", sec);
            }
            if let Some(p) = peer {
                child_str("peer", p);
            }
        }
        SnapKind::Port { port } => {
            let mut p = KdlNode::new("port");
            p.push(KdlEntry::new(*port as i128));
            ch.nodes_mut().push(p);
        }
        _ => {}
    }
    ch.nodes_mut().push(node2("pos", s.pos[0], s.pos[1]));
    ch.nodes_mut().push(node2("size", s.size[0], s.size[1]));
    if let SnapKind::App { options, args, .. } = &s.kind {
        if !options.is_empty() {
            let mut opts = KdlNode::new("options");
            for &v in options {
                opts.push(KdlEntry::new(v as f64));
            }
            ch.nodes_mut().push(opts);
        }
        if !args.is_empty() {
            let mut a = KdlNode::new("args");
            for arg in args {
                a.push(str_entry(arg));
            }
            ch.nodes_mut().push(a);
        }
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
        description: None,
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
        match &dep.description {
            Some(d) => println!("  {}  {}  — {d}", dep.name, dep.source.to_kdl()),
            None => println!("  {}  {}", dep.name, dep.source.to_kdl()),
        }
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
                    description: Some("spinning demo triangle".into()),
                },
                Dependency {
                    name: "fetch".into(),
                    source: Source::Oci("ghcr.io/o/fetch:1".into()),
                    args: vec!["example.com".into(), "80".into()],
                    description: None,
                },
            ],
            workspaces: vec![
                Workspace {
                    id: wa,
                    nodes: vec![
                        NodeSnap {
                            id: synth,
                            pos: [40.0, 56.0],
                            size: [360.0, 260.0],
                            kind: SnapKind::App {
                                name: "synth".into(),
                                options: vec![8.0, 0.6, 0.0, 1.0],
                                args: vec!["netserve".into(), "80".into()],
                            },
                        },
                        NodeSnap {
                            id: chan,
                            pos: [200.0, 120.0],
                            size: [130.0, 44.0],
                            kind: SnapKind::VirtualFile {
                                name: "chan".into(),
                            },
                        },
                        NodeSnap {
                            id: notes,
                            pos: [200.0, 200.0],
                            size: [130.0, 44.0],
                            kind: SnapKind::HostFile {
                                path: "notes.txt".into(),
                            },
                        },
                        NodeSnap {
                            id: port,
                            pos: [600.0, 100.0],
                            size: [130.0, 44.0],
                            kind: SnapKind::Port { port: 8080 },
                        },
                        NodeSnap {
                            id: net,
                            pos: [700.0, 100.0],
                            size: [130.0, 44.0],
                            kind: SnapKind::Net { gateway: false },
                        },
                        NodeSnap {
                            id: gw,
                            pos: [700.0, 200.0],
                            size: [130.0, 44.0],
                            kind: SnapKind::Net { gateway: true },
                        },
                        NodeSnap {
                            id: mdst,
                            pos: [800.0, 100.0],
                            size: [130.0, 44.0],
                            kind: SnapKind::Iroh {
                                secret: Some(secret_hex(&[7u8; 32])),
                                peer: Some("endpointabc123".into()),
                            },
                        },
                        NodeSnap {
                            id: msrc,
                            pos: [900.0, 100.0],
                            size: [130.0, 44.0],
                            kind: SnapKind::Veilid {
                                secret: Some("VLD0:pubkey:secretkey".into()),
                                peer: Some("VLD0:remoterecordkey".into()),
                            },
                        },
                    ],
                    connections: vec![(chan, synth)],
                    midi: vec![(msrc, mdst)],
                    serves: vec![(synth, port)],
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
        assert_eq!(
            back.dependencies[0].description.as_deref(),
            Some("spinning demo triangle")
        );
        assert_eq!(back.dependencies[1].description, None);
        assert_eq!(back.dependencies[1].args, vec!["example.com", "80"]);
        assert!(matches!(back.dependencies[1].source, Source::Oci(_)));

        // Nodes of every kind, wiring, and order survive exactly.
        assert_eq!(back, doc);
    }

    #[test]
    fn hostport_out_of_range_port_is_rejected_not_truncated() {
        let ws = NodeId::from_u128(1);
        let hp = NodeId::from_u128(2);
        let text = |port: u32| {
            format!(
                "workspace \"{ws}\" {{\n    \
                 hostport \"{hp}\" {{ port {port}; pos 0 0; size 10 10 }}\n}}"
            )
        };
        // 99999 doesn't fit in a u16; the node is dropped, not truncated to 34463.
        let doc = Document::from_kdl(&text(99999)).expect("parses");
        assert!(doc.workspaces[0].nodes.is_empty());
        // A valid port is kept as-is.
        let doc = Document::from_kdl(&text(8080)).expect("parses");
        assert_eq!(
            doc.workspaces[0].nodes[0].kind,
            SnapKind::Port { port: 8080 }
        );
    }

    // ---- property-based round-trip ----
    //
    // For every document the generator can produce, `parse(format(doc)) == doc`.
    // The generators below deliberately stay inside the format's domain (finite
    // coordinates, identifier-shaped dependency names) so a failure means a real
    // serialization/parse asymmetry rather than an out-of-domain input.

    use proptest::prelude::*;

    /// Any id, derived deterministically from a `u128` so proptest can shrink it.
    fn any_node_id() -> impl Strategy<Value = NodeId> {
        any::<u128>().prop_map(NodeId::from_u128)
    }

    /// A finite canvas coordinate / knob value. Excludes NaN and infinities (not
    /// representable in the file) and magnitudes past ~1e6, beyond which the KDL
    /// numeric form is not the concern of this test.
    fn coord() -> impl Strategy<Value = f32> {
        -1.0e6f32..=1.0e6f32
    }

    /// A string stored as a KDL *value* (node/file name, arg). Mixes ordinary
    /// text with cases that stress the serializer: number/keyword-shaped strings
    /// that a naive formatter emits unquoted, and characters that must be escaped
    /// inside a quoted string.
    fn value_str() -> impl Strategy<Value = String> {
        prop_oneof![
            6 => "[a-zA-Z0-9 ._+-]{0,12}",
            1 => Just("-.0".to_string()),
            1 => Just("true".to_string()),
            1 => Just(r#""quo\te""#.to_string()),
            1 => Just("line\nbreak\ttab".to_string()),
            1 => Just(String::new()),
        ]
    }

    /// A dependency name, which becomes a KDL *node name*. Restricted to bare
    /// identifiers: the parser intentionally trims a trailing `:` (npm-style), so
    /// names ending in `:` are not round-trip identities and are excluded here.
    fn dep_name() -> impl Strategy<Value = String> {
        "[a-zA-Z][a-zA-Z0-9_-]{0,10}"
    }

    fn source() -> impl Strategy<Value = Source> {
        prop_oneof![
            // No ':' in the path alphabet, so a path can never look like `oci://…`.
            "[a-zA-Z0-9_./-]{1,20}".prop_map(|s| Source::Path(PathBuf::from(s))),
            "[a-z0-9][a-z0-9./:-]{0,20}".prop_map(Source::Oci),
        ]
    }

    fn dependency() -> impl Strategy<Value = Dependency> {
        (
            dep_name(),
            source(),
            prop::collection::vec(value_str(), 0..3),
            prop::option::of(value_str()),
        )
            .prop_map(|(name, source, args, description)| Dependency {
                name,
                source,
                args,
                description,
            })
    }

    fn uplink_fields() -> impl Strategy<Value = (Option<String>, Option<String>)> {
        (
            prop::option::of(
                prop::collection::vec(any::<u8>(), 32)
                    .prop_map(|s| secret_hex(&<[u8; 32]>::try_from(s.as_slice()).unwrap())),
            ),
            prop::option::of(value_str()),
        )
    }

    fn snap_kind() -> impl Strategy<Value = SnapKind> {
        prop_oneof![
            (
                value_str(),
                prop::collection::vec(coord(), 0..4),
                prop::collection::vec(value_str(), 0..3),
            )
                .prop_map(|(name, options, args)| SnapKind::App {
                    name,
                    options,
                    args
                }),
            value_str().prop_map(|name| SnapKind::VirtualFile { name }),
            value_str().prop_map(|p| SnapKind::HostFile {
                path: PathBuf::from(p)
            }),
            any::<u16>().prop_map(|port| SnapKind::Port { port }),
            any::<bool>().prop_map(|gateway| SnapKind::Net { gateway }),
            uplink_fields().prop_map(|(secret, peer)| SnapKind::Iroh { secret, peer }),
            uplink_fields().prop_map(|(secret, peer)| SnapKind::Veilid { secret, peer }),
        ]
    }

    fn node_snap() -> impl Strategy<Value = NodeSnap> {
        (
            any_node_id(),
            coord(),
            coord(),
            coord(),
            coord(),
            snap_kind(),
        )
            .prop_map(|(id, px, py, sx, sy, kind)| NodeSnap {
                id,
                pos: [px, py],
                size: [sx, sy],
                kind,
            })
    }

    fn pair() -> impl Strategy<Value = (NodeId, NodeId)> {
        (any_node_id(), any_node_id())
    }

    fn workspace_strat() -> impl Strategy<Value = Workspace> {
        (
            any_node_id(),
            prop::collection::vec(node_snap(), 0..6),
            prop::collection::vec(pair(), 0..3),
            prop::collection::vec(pair(), 0..3),
            prop::collection::vec(pair(), 0..3),
            prop::collection::vec(pair(), 0..3),
        )
            .prop_map(|(id, nodes, conns, midi, serves, netlinks)| Workspace {
                id,
                nodes,
                connections: conns,
                midi,
                serves,
                net_links: netlinks,
            })
    }

    fn document() -> impl Strategy<Value = Document> {
        (
            prop::collection::vec(dependency(), 0..3),
            // A document always has at least one workspace.
            prop::collection::vec(workspace_strat(), 1..3),
        )
            .prop_map(|(dependencies, workspaces)| Document {
                dependencies,
                workspaces,
            })
    }

    proptest! {
        #[test]
        fn document_kdl_round_trips_for_any_document(doc in document()) {
            let text = doc.to_kdl();
            let back = Document::from_kdl(&text)
                .map_err(|e| TestCaseError::fail(format!("re-parse failed: {e}")))?;
            prop_assert_eq!(back, doc);
        }
    }
}
