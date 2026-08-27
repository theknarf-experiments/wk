//! The wk **workspace file**: a `.wk` file (KDL syntax; `workspace.wk` by
//! default) holding a project's shared *dependencies* plus one or more
//! *workspaces* (canvas tabs), each with its own id, nodes, and wiring. It can
//! also `import` other `.wk` files, pulling in their dependencies and
//! workspaces (recursively) — so a project can split a shared dependency list
//! from the setups that use it. See [`Document::load_resolved`].
//!
//! ```kdl
//! import "../deps.wk"        // pull in another file's deps + workspaces
//! dependencies {
//!     triangle "plugins/triangle/.../triangle.wasm"
//!     foo      "oci://ghcr.io/org/foo:1.0"
//! }
//! workspace "0000000000000000000000000M" {
//!     node "synth" "0000000000000000000000000N" { pos 19 88; size 360 260 }
//!     midi "0000000000000000000000000N" "0000000000000000000000000P"
//! }
//! ```

use crate::server::{FILE_H, FILE_W};
use kdl::{KdlDocument, KdlEntry, KdlEntryFormat, KdlNode, KdlValue};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use wk_protocol::{NodeId, PortDir, PortKind};

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

/// Which argument of a placed-node line carries the node id. Most kinds lead
/// with it (`network "<id>"`); the named kinds put their own name or path first
/// (`node "synth" "<id>"`, `group "voice" "<id>"`); a boundary port declares a
/// name *and* a connection kind before it (`inport "notes" "midi" "<id>"`).
/// Both [`parse_snap`] and [`node_ident`] ask this one function — they used to
/// keep separate lists, which drifted apart and silently cost a `hostservice`
/// line its comments on every save.
fn id_arg_index(keyword: &str) -> usize {
    match keyword {
        "node" | "volume" | "virtualfile" | "bindmount" | "hostfile" | "midiin" | "hostservice"
        | "group" => 1,
        "inport" | "outport" => 2,
        _ => 0,
    }
}

/// The identity that pairs a freshly-generated KDL node with the same node in
/// the existing file, so a save can carry that node's comment across. Placed
/// nodes and workspaces key on their stable id; wires on their kind + endpoints.
#[derive(PartialEq, Eq, Hash)]
enum NodeIdent {
    Import(String),
    Dependencies,
    Workspace(NodeId),
    /// A workspace's `name "voice"` line. There is at most one per workspace,
    /// so the keyword alone identifies it — and it must have an identity, or a
    /// comment written above a workspace's name vanishes on the first save.
    WorkspaceName,
    /// A workspace's `tab #false` line; like [`NodeIdent::WorkspaceName`],
    /// unique within its block and in need of an identity to keep its comment.
    WorkspaceTab,
    Placed(NodeId),
    Wire(String, NodeId, NodeId),
    /// A boundary wire inside a `group` block (`in "notes" "<node id>"`). Its
    /// port name and the node it joins identify it within the block, so a
    /// comment written above one survives a save like any other line's.
    GroupWire(String, String, NodeId),
}

/// The identity of a top-level or in-workspace KDL node, if it has one.
fn node_ident(n: &KdlNode) -> Option<NodeIdent> {
    let name = n.name().value();
    match name {
        "import" => n
            .get(0)
            .and_then(|v| v.as_string())
            .map(|s| NodeIdent::Import(s.to_string())),
        "dependencies" => Some(NodeIdent::Dependencies),
        "workspace" => n.get(0).and_then(node_id).map(NodeIdent::Workspace),
        "name" => Some(NodeIdent::WorkspaceName),
        "tab" => Some(NodeIdent::WorkspaceTab),
        "connection" | "midi" | "serve" | "netlink" | "capturelink" | "clipboardlink"
        | "apilink" => {
            let a = n.get(0).and_then(node_id)?;
            let b = n.get(1).and_then(node_id)?;
            Some(NodeIdent::Wire(name.to_string(), a, b))
        }
        // Only ever seen inside a `group` block; at workspace level neither
        // word names a node, so nothing else can collide with it.
        "in" | "out" => {
            let port = n.get(0).and_then(|v| v.as_string())?;
            let node = n.get(1).and_then(node_id)?;
            Some(NodeIdent::GroupWire(
                name.to_string(),
                port.to_string(),
                node,
            ))
        }
        // A placed node, boundary ports included: its id is its identity,
        // wherever on the line it sits.
        _ => node_id(n.get(id_arg_index(name))?).map(NodeIdent::Placed),
    }
}

/// Graft the comments from `old` onto the freshly-built `fresh` document:
/// the header (document leading trivia) and each node's leading comment,
/// matched by [`node_ident`], recursing into workspace/dependency children.
/// `autoformat` (called after) keeps these comments while normalising spacing.
fn graft_comments(fresh: &mut KdlDocument, old: &KdlDocument) {
    if let Some(hdr) = old.format().map(|f| f.leading.clone()) {
        if !hdr.trim().is_empty() {
            let mut f = fresh.format().cloned().unwrap_or_default();
            f.leading = hdr;
            fresh.set_format(f);
        }
    }
    graft_level(fresh, old);
}

/// Copy leading comments from `old`'s nodes onto `fresh`'s at one nesting level,
/// recursing into the children of any node paired by identity.
fn graft_level(fresh: &mut KdlDocument, old: &KdlDocument) {
    use std::collections::HashMap;
    let old_by: HashMap<NodeIdent, &KdlNode> = old
        .nodes()
        .iter()
        .filter_map(|n| node_ident(n).map(|i| (i, n)))
        .collect();
    for fnode in fresh.nodes_mut() {
        let Some(ident) = node_ident(fnode) else {
            continue;
        };
        let Some(&onode) = old_by.get(&ident) else {
            continue;
        };
        if let Some(of) = onode.format() {
            if !of.leading.trim().is_empty() {
                let mut ff = fnode.format().cloned().unwrap_or_default();
                ff.leading = of.leading.clone();
                fnode.set_format(ff);
            }
        }
        if let (Some(oc), Some(fc)) = (onode.children(), fnode.children_mut().as_mut()) {
            graft_level(fc, oc);
        }
    }
}

/// Where a dependency's wasm comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Path(PathBuf),
    /// An OCI registry reference (e.g. `ghcr.io/org/name:1.0`), pulled + cached.
    Oci(String),
    /// A Dockerfile to build into a local OCI image (`docker://<path>`): the
    /// entrypoint wasm runs as the node, the image's layers become its rootfs.
    Dockerfile(PathBuf),
    /// A locally-built/pulled image referenced by name (`image://<tag-or-id>`),
    /// resolved against the local image store's tags.
    Image(String),
}

impl Source {
    /// Parse the string form stored in the workspace file: `oci://` = a registry
    /// artifact, `docker://` = a Dockerfile to build, `image://` = a local image
    /// by tag/id, else a plain wasm path.
    pub fn parse(s: &str) -> Self {
        if let Some(reference) = s.strip_prefix("oci://") {
            return Source::Oci(reference.to_string());
        }
        if let Some(path) = s.strip_prefix("docker://") {
            return Source::Dockerfile(PathBuf::from(path));
        }
        if let Some(reference) = s.strip_prefix("image://") {
            return Source::Image(reference.to_string());
        }
        Source::Path(PathBuf::from(s))
    }

    pub fn to_kdl(&self) -> String {
        match self {
            Source::Path(p) => p.to_string_lossy().into_owned(),
            Source::Oci(reference) => format!("oci://{reference}"),
            Source::Dockerfile(p) => format!("docker://{}", p.to_string_lossy()),
            Source::Image(reference) => format!("image://{reference}"),
        }
    }

    /// The local path to load the wasm from. For OCI this is the cached
    /// content-addressed blob, for a Dockerfile the built image's extracted
    /// entrypoint (both populated by [`Source::ensure`]); it may not exist
    /// until then.
    pub fn local_path(&self) -> PathBuf {
        match self {
            Source::Path(p) => p.clone(),
            Source::Oci(reference) => crate::oci::cached_artifact(reference)
                .unwrap_or_else(|| crate::oci::legacy_ref_path(reference)),
            Source::Dockerfile(p) => crate::images::aliased_image(p)
                .map(|(id, _)| crate::images::entrypoint_path(&id))
                .unwrap_or_else(|| crate::images::entrypoint_path("unbuilt")),
            Source::Image(reference) => crate::images::resolve_ref(reference)
                .map(|id| crate::images::entrypoint_path(&id))
                .unwrap_or_else(|| crate::images::entrypoint_path("unresolved")),
        }
    }

    /// Make the source runnable: pull + cache an OCI artifact, (re)build a
    /// Dockerfile image, or verify a local `image://` ref resolves. A no-op for
    /// local paths.
    pub fn ensure(&self) -> Result<(), String> {
        match self {
            Source::Oci(reference) => {
                if crate::oci::cached_artifact(reference).is_none() {
                    println!("pulling {reference} ...");
                    crate::oci::pull_into_cache(reference)?;
                }
                Ok(())
            }
            Source::Dockerfile(p) => {
                let id = crate::images::build_and_alias(p, false)?;
                println!("built {} -> {id}", p.display());
                Ok(())
            }
            Source::Image(reference) => crate::images::resolve_ref(reference)
                .map(|_| ())
                .ok_or_else(|| format!("no local image {reference:?} (see `wk images list`)")),
            Source::Path(_) => Ok(()),
        }
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

    /// The built image behind a `docker://` source — the layers to mount and
    /// the guest env — if this dependency is one (and it has been built).
    pub fn container(&self) -> Option<crate::images::ContainerSetup> {
        match &self.source {
            Source::Dockerfile(p) => {
                crate::images::aliased_image(p).map(|(_, m)| m.container_setup())
            }
            // A pulled container image stores under its sanitized reference; a
            // plain wasm artifact has no stored image and mounts nothing.
            Source::Oci(reference) => crate::images::load_image(&crate::oci::sanitize(reference))
                .map(|m| m.container_setup()),
            Source::Image(reference) => crate::images::resolve_ref(reference)
                .and_then(|id| crate::images::load_image(&id))
                .map(|m| m.container_setup()),
            Source::Path(_) => None,
        }
    }

    /// The dependency's default launch args: its own, or — for an image with
    /// none set — the image's ENTRYPOINT[1..] + CMD.
    pub fn effective_args(&self) -> Vec<String> {
        if !self.args.is_empty() {
            return self.args.clone();
        }
        match &self.source {
            Source::Dockerfile(p) => crate::images::aliased_image(p)
                .map(|(_, m)| m.default_args())
                .unwrap_or_default(),
            Source::Oci(reference) => crate::images::load_image(&crate::oci::sanitize(reference))
                .map(|m| m.default_args())
                .unwrap_or_default(),
            Source::Image(reference) => crate::images::resolve_ref(reference)
                .and_then(|id| crate::images::load_image(&id))
                .map(|m| m.default_args())
                .unwrap_or_default(),
            Source::Path(_) => Vec::new(),
        }
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
    /// Optional free 3D pose in the world: `[x, y, z, yaw]` (world units,
    /// radians). Absent = the node sits on the default layout cylinder.
    pub pos3d: Option<[f32; 4]>,
    /// Whether the node's flat 2D panel is drawn in the 3D world. `false`
    /// (written as `panel3d #false`) leaves a `wk:scene` node as its 3D object
    /// alone. Default `true` — the panel is how most nodes are visible at all.
    pub panel3d: bool,
    pub kind: SnapKind,
}

/// The kind-specific part of a [`NodeSnap`]. Each variant serializes under its
/// own KDL node name (`node`, `volume`, `bindmount`, `hostport`,
/// `network`/`gateway`, `iroh`, `veilid`). The legacy names `virtualfile`
/// and `hostfile` are still accepted on read.
#[derive(Clone, Debug, PartialEq)]
pub enum SnapKind {
    /// An app node; `name` resolves against the dependency list.
    App {
        /// Which dependency this node runs — its *type*, the way an image name
        /// is a container's type. Several nodes can share one.
        dep: String,
        /// What this node is *called*: its identity on the fabric, in `wk ps`,
        /// and to every CLI verb that takes a node. Absent means "the same as
        /// the type", which is what a workspace holding one of each wants; a
        /// second node of a type is named when it is created, and that name is
        /// written here and never changes on its own afterwards.
        name: Option<String>,
        /// Option values (knob settings), persisted positionally.
        options: Vec<f32>,
        /// Launch args, editable in the GUI. Empty falls back to the
        /// dependency's default args at materialization.
        args: Vec<String>,
        /// A *custom* capability token (hex-encoded Biscuit), set via
        /// `wk token`. Absent = the workspace's default node token, which is
        /// minted fresh each run and never persisted.
        token: Option<String>,
    },
    /// An in-memory named volume. Its *bytes* are runtime state — undo carries
    /// them alongside the snap; the `.wk` file persists them (to a sidecar) only
    /// when `persist` is set, otherwise the volume is empty each run.
    Volume { name: String, persist: bool },
    /// A disk-backed file node (its mount name derives from the path).
    BindMount { path: PathBuf },
    /// A localhost HostPort.
    Port { port: u16 },
    /// A Network node (or Gateway — a Network granting host access).
    Net { gateway: bool },
    /// A Router node: wired to two or more Networks, it lets their members
    /// reach each other while each node keeps the single network it is on.
    Router,
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
    /// A yellow sticky note: purely visual annotation, wired to nothing.
    Note { text: String },
    /// A Screen Capture capability node: apps wired to it may read captured
    /// frames (see the `capturelink` pairs).
    Capture,
    /// A Clipboard capability node: apps wired to it may read and/or write the
    /// HOST's system clipboard (see the `clipboardlink` pairs). Which of the
    /// two a given app may do is a token decision, not a file one — the wire
    /// is the grant, the token attenuates it.
    Clipboard,
    /// The wk client API as a node: apps wired to it may drive wk over their
    /// virtual network (see the `apilink` pairs).
    Api,
    /// A hardware MIDI input node: the host opens the named device (empty = the
    /// first available) and routes its messages to the apps it's wired to.
    MidiIn { device: String },
    /// A host TCP service published into the Network it's wired to, as fabric
    /// peer `name`; connections bridge to the host `target` (`addr:port`).
    HostService { name: String, target: String },
    /// A workspace **boundary in-port**: the named, typed edge through which a
    /// connection enters this workspace. It is a placed node so it can be
    /// wired to inner nodes with ordinary wire lines — the wiring is what
    /// typechecks the boundary and what says how far in a connection reaches.
    ///
    /// It stands in for whatever supplies the connection from outside (the
    /// volume of a bind, the source of a MIDI link, the capability node of a
    /// grant), so in a plain tab it stands in for nothing and does nothing.
    InPort { name: String, kind: PortKind },
    /// A workspace **boundary out-port**: the mirror of [`SnapKind::InPort`],
    /// standing in for the *consumer* on the far side — what an inner node's
    /// connection reaches once this workspace is used from elsewhere.
    OutPort { name: String, kind: PortKind },
    /// An **instance** of another workspace: everything the definition named by
    /// `definition` (a workspace's `name`, with `tab #false`) contains, stamped
    /// into this canvas with ids derived from this node's own id. Written
    /// `group "voice" "<instance id>"`.
    ///
    /// The instance id is what makes two copies of one definition distinct, so
    /// it is what the id derivation keys on — see [`crate::instancing`], which
    /// resolves the name and computes the ids.
    Group {
        /// Which definition this instantiates — its *type*, exactly as an app
        /// node's `dep` is. Several instances can share one.
        definition: String,
        /// What this instance is *called*. Absent means "the same as the
        /// definition"; a second instance of one is named when it is loaded,
        /// and every node inside an instance is named after it, so this is the
        /// scope that makes an instance's members addressable at all.
        name: Option<String>,
        /// `in "<port name>" "<node id>"`: a node on *this* canvas feeding one
        /// of the definition's in-ports.
        ///
        /// Carried, not yet interpreted. Nothing expands a group into live
        /// nodes yet, and typechecking a boundary wire against the port it
        /// names belongs with the code that does — but dropping the lines in
        /// the meantime would quietly lose the wiring the author wrote.
        in_wires: Vec<(String, NodeId)>,
        /// `out "<port name>" "<node id>"`: what one of the definition's
        /// out-ports reaches on this canvas. See `in_wires`.
        out_wires: Vec<(String, NodeId)>,
    },
}

impl SnapKind {
    /// A boundary port's direction and connection kind, if this is one.
    pub fn boundary(&self) -> Option<(PortDir, PortKind)> {
        match self {
            SnapKind::InPort { kind, .. } => Some((PortDir::In, *kind)),
            SnapKind::OutPort { kind, .. } => Some((PortDir::Out, *kind)),
            _ => None,
        }
    }
}

/// Hex-encode an uplink secret for persistence.
pub fn secret_hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hex-encode arbitrary bytes (capability tokens) for persistence/transport.
pub fn bytes_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode hex back to bytes, if well-formed.
pub fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
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

/// A `.wk` file: optional imports of other `.wk` files, shared dependencies, and
/// one or more workspaces (canvas tabs).
///
/// A file loaded on its own (via [`Document::load`]) carries only what it
/// literally contains, with empty provenance. [`Document::load_resolved`]
/// follows `imports` and returns a *merged* document — `dependencies` and
/// `workspaces` include everything pulled in — while recording which of those
/// came from an import in `imported_deps`/`imported_workspaces`. Serialization
/// ([`to_kdl`](Document::to_kdl)) writes the `import` lines and *omits* imported
/// content, so an autosave (or a CLI edit) preserves the composition instead of
/// flattening it into one file.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    /// Paths (relative to this file) of other `.wk` files to pull in.
    pub imports: Vec<String>,
    pub dependencies: Vec<Dependency>,
    /// Always at least one, and — once resolved for running — always at least
    /// one that is a tab. Shown as tabs when there is more than one.
    pub workspaces: Vec<Workspace>,
    /// Names of `dependencies` that came from an import — not re-serialized.
    /// Empty unless produced by [`Document::load_resolved`].
    pub imported_deps: std::collections::HashSet<String>,
    /// Ids of `workspaces` that came from an import — not re-serialized.
    pub imported_workspaces: std::collections::HashSet<NodeId>,
    /// The tab [`Self::load_resolved`] invented because the document had none —
    /// a place to stand in a file that is nothing but definitions. Provenance,
    /// like the two sets above: the user never wrote it, so as long as it is
    /// still empty nothing may write it back (see [`crate::server::Server::save`]).
    pub scratch_tab: Option<NodeId>,
}

/// One workspace: a canvas of nodes and the wiring between them, with its own id.
#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub id: NodeId,
    /// A human name for the tab (`name "voice"`), shown instead of its number.
    /// `None` (absent) and `Some("")` (written, but blank) are distinct: only
    /// the former means "this workspace was never named".
    pub name: Option<String>,
    /// Whether this workspace runs standalone as a canvas tab. `false` (written
    /// as `tab #false`) makes it a *definition*: content that exists to be used
    /// from elsewhere, not to be opened. The server never instantiates one and
    /// clients never list it — so its content is authored-only, and
    /// [`crate::server::Server`] has to carry it verbatim across a save.
    pub tab: bool,
    /// Every placed node, of any kind (each serializes under its kind's KDL
    /// node name). File order is preserved.
    pub nodes: Vec<NodeSnap>,
    /// Volume binds as (volume id, app node id).
    pub connections: Vec<(NodeId, NodeId)>,
    /// Where a bind mounts inside its app, keyed by (volume, app). Absent = the
    /// default (the volume's name at the filesystem root). Only overrides are
    /// stored, so a fresh bind adds nothing here.
    pub mount_paths: BTreeMap<(NodeId, NodeId), String>,
    /// MIDI links as (source, destination).
    pub midi: Vec<(NodeId, NodeId)>,
    /// Serve wiring as (served node id, HostPort id).
    pub serves: Vec<(NodeId, NodeId)>,
    /// The guest (container) port a serve wire forwards to, keyed by
    /// (served, hostport). Absent = the HostPort's own port. Only overrides are
    /// stored (a `host:container` mapping where the two differ).
    pub serve_ports: BTreeMap<(NodeId, NodeId), u16>,
    /// Network membership as (member id, Network id).
    pub net_links: Vec<(NodeId, NodeId)>,
    /// Screen-capture grants as (app id, Capture node id).
    pub capture_links: Vec<(NodeId, NodeId)>,
    /// Host-clipboard grants as (app id, Clipboard node id). A `.wk` file
    /// written before clipboard existed simply has none, and loads unchanged.
    pub clipboard_links: Vec<(NodeId, NodeId)>,
    /// API grants as (app id, Api node id).
    pub api_links: Vec<(NodeId, NodeId)>,
}

impl Workspace {
    /// A fresh, empty workspace with a new id.
    pub fn new() -> Self {
        Workspace {
            id: NodeId::new(),
            name: None,
            tab: true,
            nodes: Vec::new(),
            connections: Vec::new(),
            mount_paths: BTreeMap::new(),
            midi: Vec::new(),
            serves: Vec::new(),
            serve_ports: BTreeMap::new(),
            net_links: Vec::new(),
            capture_links: Vec::new(),
            clipboard_links: Vec::new(),
            api_links: Vec::new(),
        }
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    /// An empty document: no imports or dependencies, one blank workspace.
    pub fn empty() -> Self {
        Document {
            imports: Vec::new(),
            dependencies: Vec::new(),
            workspaces: vec![Workspace::new()],
            imported_deps: std::collections::HashSet::new(),
            imported_workspaces: std::collections::HashSet::new(),
            scratch_tab: None,
        }
    }

    /// Load a single `.wk` file verbatim — its own imports/dependencies/
    /// workspaces, with empty provenance. Used by the CLI edit commands (which
    /// operate on one file) and by [`Self::load_resolved`] per file. Does *not*
    /// follow imports; see [`Self::load_resolved`] for the merged, runnable view.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "no {} in this directory ({e}); create one with `wk init`",
                path.display()
            )
        })?;
        Self::from_kdl(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Load `path` and everything it `import`s, recursively, into one merged
    /// document for running: `dependencies` and `workspaces` include all pulled-
    /// in content, with `imported_deps`/`imported_workspaces` recording what came
    /// from an import (so a later save doesn't flatten the composition). Imports
    /// are resolved relative to the importing file; a file already pulled in
    /// (a diamond, or a cycle) is visited once. The top-level file's own
    /// `imports` are preserved for re-serialization.
    pub fn load_resolved(path: &Path) -> Result<Self, String> {
        let mut merged = Document {
            imports: Vec::new(),
            dependencies: Vec::new(),
            workspaces: Vec::new(),
            imported_deps: std::collections::HashSet::new(),
            imported_workspaces: std::collections::HashSet::new(),
            scratch_tab: None,
        };
        let mut dep_names = std::collections::HashSet::new();
        let mut ws_ids = std::collections::HashSet::new();
        let mut visited = std::collections::HashSet::new();
        resolve_into(
            path,
            false,
            &mut merged,
            &mut dep_names,
            &mut ws_ids,
            &mut visited,
        )?;
        // At least one *tab*, not merely one workspace: clients render the tab
        // list straight from this, and `wk ps` fails outright on a document with
        // none. A file that is nothing but definitions (every workspace
        // `tab #false`) therefore gets a fresh scratch tab to open. Flipping an
        // authored `tab #false` instead would be the cheaper fix and the wrong
        // one — save re-projects this model, so it would quietly rewrite the
        // user's file into something that no longer says what they wrote.
        if !merged.workspaces.iter().any(|w| w.tab) {
            let scratch = Workspace::new();
            merged.scratch_tab = Some(scratch.id);
            merged.workspaces.push(scratch);
        }
        Ok(merged)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        // A from-scratch `to_kdl` would drop every comment in the file. Instead,
        // rebuild the body from the model but graft the *existing* file's
        // comments back onto the matching nodes (kdl-rs is format-preserving, so
        // `autoformat` keeps them) — the header, per-node notes, and formatting
        // all survive, and an unchanged node produces byte-identical output.
        let text = match std::fs::read_to_string(path)
            .ok()
            .and_then(|s| KdlDocument::parse(&s).ok())
        {
            Some(old) => {
                let mut doc = self.to_kdl_doc();
                graft_comments(&mut doc, &old);
                doc.autoformat();
                let out = doc.to_string();
                // A file without the modeline (or grafted from one lacking it)
                // still gets it, so `.wk` keeps its editor highlighting.
                if out.trim_start().starts_with(MODELINE) {
                    out
                } else {
                    format!("{MODELINE}\n{out}")
                }
            }
            None => self.to_kdl(), // new or unparseable file: fresh rebuild
        };
        std::fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))
    }

    fn from_kdl(text: &str) -> Result<Self, String> {
        let doc: KdlDocument = text.parse().map_err(|e| format!("parse error: {e}"))?;
        validate_boundaries(&doc)?;

        // `import "other.wk"` lines (a path per node, resolved by load_resolved).
        let imports = doc
            .nodes()
            .iter()
            .filter(|n| n.name().value() == "import")
            .filter_map(|n| n.get(0).and_then(|v| v.as_string()).map(str::to_string))
            .collect();

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
            imports,
            dependencies,
            workspaces,
            imported_deps: std::collections::HashSet::new(),
            imported_workspaces: std::collections::HashSet::new(),
            scratch_tab: None,
        })
    }

    /// Serialize to KDL text: a fresh rebuild from the model (used for a new
    /// file). [`Self::save`] instead grafts the existing file's comments back on.
    fn to_kdl(&self) -> String {
        let mut doc = self.to_kdl_doc();
        doc.autoformat();
        format!("{MODELINE}\n{doc}")
    }

    /// Build the KDL document from the model (imports, dependencies, workspaces),
    /// without the modeline or autoformatting — the shared body of [`Self::to_kdl`]
    /// and the comment-preserving [`Self::save`].
    fn to_kdl_doc(&self) -> KdlDocument {
        let mut doc = KdlDocument::new();

        // Import lines first, so the file reads top-down: what it pulls in, then
        // what it adds. Imported deps/workspaces are omitted below (they live in
        // the imported files); a raw single-file document has empty provenance,
        // so nothing is filtered.
        for imp in &self.imports {
            let mut node = KdlNode::new("import");
            node.push(str_entry(imp));
            doc.nodes_mut().push(node);
        }

        let mut deps = KdlNode::new("dependencies");
        let mut children = KdlDocument::new();
        for dep in self
            .dependencies
            .iter()
            .filter(|d| !self.imported_deps.contains(&d.name))
        {
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
        // Omit an empty `dependencies` block (e.g. a file that only imports).
        if !children.nodes().is_empty() {
            deps.set_children(children);
            doc.nodes_mut().push(deps);
        }

        for ws in self
            .workspaces
            .iter()
            .filter(|w| !self.imported_workspaces.contains(&w.id))
        {
            doc.nodes_mut().push(workspace_kdl(ws));
        }

        doc
    }
}

/// Recursively merge `path` (and its imports) into `merged`. `is_import` is
/// false for the top-level file (its content is "own") and true for anything
/// pulled in via `import`. Dedup: a dependency name / workspace id already seen
/// wins (so a local definition overrides an imported one, since a file's own
/// content is merged before recursing into its imports).
fn resolve_into(
    path: &Path,
    is_import: bool,
    merged: &mut Document,
    dep_names: &mut std::collections::HashSet<String>,
    ws_ids: &mut std::collections::HashSet<NodeId>,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Result<(), String> {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canon) {
        return Ok(()); // already pulled in (a diamond, or a cycle)
    }
    let doc = Document::load(path)?;
    if !is_import {
        merged.imports = doc.imports.clone();
    }
    for dep in doc.dependencies {
        if dep_names.insert(dep.name.clone()) {
            if is_import {
                merged.imported_deps.insert(dep.name.clone());
            }
            merged.dependencies.push(dep);
        }
    }
    for ws in doc.workspaces {
        // A deps-only file gets an auto-added blank workspace; don't let an
        // import contribute a phantom empty tab. A *named* workspace, or one
        // that opts out of being a tab, is never phantom — someone wrote that
        // down deliberately — so it always comes through.
        let empty = ws.name.is_none()
            && ws.tab
            && ws.nodes.is_empty()
            && ws.connections.is_empty()
            && ws.midi.is_empty()
            && ws.serves.is_empty()
            && ws.net_links.is_empty();
        if is_import && empty {
            continue;
        }
        if ws_ids.insert(ws.id) {
            if is_import {
                merged.imported_workspaces.insert(ws.id);
            }
            merged.workspaces.push(ws);
        }
    }
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    for imp in doc.imports {
        resolve_into(&base.join(&imp), true, merged, dep_names, ws_ids, visited)?;
    }
    Ok(())
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

/// Check every `inport`/`outport`/`group` line before the document becomes a
/// model.
///
/// These are read from the raw KDL, not from the parsed workspaces, because one
/// the parser can't read is one the parser *drops* — and a silently missing
/// boundary (or a silently missing instance) is the one mistake in a definition
/// that no later error can explain. A declaration is worth a load error; the
/// rest of the format's tolerance is left alone.
fn validate_boundaries(doc: &KdlDocument) -> Result<(), String> {
    for ws in doc
        .nodes()
        .iter()
        .filter(|n| n.name().value() == "workspace")
    {
        // Names are unique *per direction*: an in-port and an out-port called
        // "notes" are two ends of the same idea, and a `group` block says which
        // it means by writing `in` or `out`.
        let mut seen: std::collections::HashSet<(&str, String)> = std::collections::HashSet::new();
        for n in ws.children().map(|ch| ch.nodes()).unwrap_or(&[]) {
            if n.name().value() == "group" {
                validate_group(n)?;
                continue;
            }
            let dir = match n.name().value() {
                d @ ("inport" | "outport") => d,
                _ => continue,
            };
            let name = n.get(0).and_then(|v| v.as_string()).ok_or_else(|| {
                format!(r#"{dir} needs a name: {dir} "notes" "midi" "<node id>""#)
            })?;
            let word = n.get(1).and_then(|v| v.as_string()).ok_or_else(|| {
                format!(
                    "{dir} {name:?} needs a connection kind ({}) as its second argument",
                    PortKind::words()
                )
            })?;
            let kind = PortKind::parse(word).ok_or_else(|| {
                format!(
                    "{dir} {name:?} has unknown connection kind {word:?}; expected one of {}",
                    PortKind::words()
                )
            })?;
            // Both refused kinds are "exactly one per node" relations, which a
            // boundary would have to *move* rather than add — silently, and
            // for a node the author of the definition can't see.
            match kind {
                PortKind::Net => {
                    return Err(format!(
                        "{dir} {name:?} is a net port, which wk does not support yet: an app \
                         belongs to exactly one network, so a net wire crossing a boundary \
                         would move an inner node off its own network instead of adding one"
                    ))
                }
                PortKind::Serve => {
                    return Err(format!(
                        "{dir} {name:?} is a serve port, which wk does not support yet: a node \
                         is served through exactly one HostPort, so a serve wire crossing a \
                         boundary would take over an inner node's existing one"
                    ))
                }
                _ => {}
            }
            if !seen.insert((dir, name.to_string())) {
                return Err(format!(
                    "two {dir}s named {name:?} in the same workspace; a boundary port's name \
                     is how a wire from outside picks it, so it must be unique per direction"
                ));
            }
        }
    }
    Ok(())
}

/// Check one `group "<definition>" "<instance id>" { in/out … }` line.
///
/// Only its *shape*: whether the name resolves to a definition at all needs the
/// whole document with its imports merged, and belongs to [`crate::instancing`]
/// — which runs when a server starts, not when a file loads, so `wk add` and
/// `wk remove` keep working on a file whose instancing is broken.
fn validate_group(n: &KdlNode) -> Result<(), String> {
    let name = n
        .get(0)
        .and_then(|v| v.as_string())
        .ok_or(r#"group needs a definition name: group "voice" "<instance id>""#)?;
    n.get(1).and_then(node_id).ok_or_else(|| {
        format!(r#"group {name:?} needs an instance id: group {name:?} "<instance id>""#)
    })?;
    for c in n.children().map(|ch| ch.nodes()).unwrap_or(&[]) {
        let dir = match c.name().value() {
            d @ ("in" | "out") => d,
            _ => continue,
        };
        let port = c.get(0).and_then(|v| v.as_string()).ok_or_else(|| {
            format!(r#"group {name:?}: {dir} needs a port name: {dir} "notes" "<node id>""#)
        })?;
        c.get(1).and_then(node_id).ok_or_else(|| {
            format!(
                "group {name:?}: {dir} {port:?} needs the id of the node on this canvas to \
                 join the port to"
            )
        })?;
    }
    Ok(())
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
            // `name ""` is a named-but-blank workspace, not an unnamed one — so
            // a present line always yields `Some`, however empty.
            "name" => {
                ws.name = Some(
                    c.get(0)
                        .and_then(|v| v.as_string())
                        .unwrap_or_default()
                        .to_string(),
                )
            }
            // Absent means "this is a tab": opting out is the exception a file
            // records, so only `tab #false` is ever written.
            "tab" => ws.tab = c.get(0).and_then(|v| v.as_bool()).unwrap_or(true),
            "connection" => {
                if let Some((a, b)) = pair(c) {
                    ws.connections.push((a, b));
                    // Optional 3rd arg: the in-app mount path for this bind.
                    if let Some(p) = c.get(2).and_then(|v| v.as_string()) {
                        ws.mount_paths.insert((a, b), p.to_string());
                    }
                }
            }
            "midi" => ws.midi.extend(pair(c)),
            "serve" => {
                if let Some((a, b)) = pair(c) {
                    ws.serves.push((a, b));
                    // Optional 3rd arg: the guest (container) port for this serve.
                    if let Some(p) = c.get(2).and_then(uint) {
                        if let Ok(p) = u16::try_from(p) {
                            ws.serve_ports.insert((a, b), p);
                        }
                    }
                }
            }
            "netlink" => ws.net_links.extend(pair(c)),
            "capturelink" => ws.capture_links.extend(pair(c)),
            "clipboardlink" => ws.clipboard_links.extend(pair(c)),
            "apilink" => ws.api_links.extend(pair(c)),
            _ => ws.nodes.extend(parse_snap(c)),
        }
    }
    Some(ws)
}

fn workspace_kdl(ws: &Workspace) -> KdlNode {
    let mut node = KdlNode::new("workspace");
    node.push(KdlEntry::new(ws.id.to_string()));
    let mut ch = KdlDocument::new();
    // The name leads the block: it is what the tab is called, so it reads first.
    if let Some(name) = &ws.name {
        let mut n = KdlNode::new("name");
        n.push(str_entry(name));
        ch.nodes_mut().push(n);
    }
    // Only a definition writes the flag; the default is a tab you can open.
    if !ws.tab {
        let mut n = KdlNode::new("tab");
        n.push(KdlEntry::new(false));
        ch.nodes_mut().push(n);
    }
    for n in &ws.nodes {
        ch.nodes_mut().push(snap_kdl(n));
    }
    for &(file, node) in &ws.connections {
        let mut c = pair_kdl("connection", file, node);
        // A non-default mount path rides along as a 3rd arg.
        if let Some(path) = ws.mount_paths.get(&(file, node)) {
            c.push(str_entry(path));
        }
        ch.nodes_mut().push(c);
    }
    for &(src, dst) in &ws.midi {
        ch.nodes_mut().push(pair_kdl("midi", src, dst));
    }
    for &(served, hostport) in &ws.serves {
        let mut s = pair_kdl("serve", served, hostport);
        // A non-default container port rides along as a 3rd arg.
        if let Some(&port) = ws.serve_ports.get(&(served, hostport)) {
            s.push(KdlEntry::new(port as i128));
        }
        ch.nodes_mut().push(s);
    }
    for &(member, net) in &ws.net_links {
        ch.nodes_mut().push(pair_kdl("netlink", member, net));
    }
    for &(app, cap) in &ws.capture_links {
        ch.nodes_mut().push(pair_kdl("capturelink", app, cap));
    }
    for &(app, clip) in &ws.clipboard_links {
        ch.nodes_mut().push(pair_kdl("clipboardlink", app, clip));
    }
    for &(app, api) in &ws.api_links {
        ch.nodes_mut().push(pair_kdl("apilink", app, api));
    }
    node.set_children(ch);
    node
}

/// Parse one placed node of any kind, dispatching on the KDL node name.
/// Unknown names yield `None` (tolerated, like any unknown entry).
fn parse_snap(n: &KdlNode) -> Option<NodeSnap> {
    let id = node_id(n.get(id_arg_index(n.name().value()))?)?;
    // A boundary port and a group instance are *declarations* first and placed
    // nodes second, so either may leave the geometry block out entirely; every
    // other kind is a canvas node that has always written one.
    let declaration = matches!(n.name().value(), "inport" | "outport" | "group");
    let empty = KdlDocument::new();
    let ch = match n.children() {
        Some(ch) => ch,
        None if declaration => &empty,
        None => return None,
    };
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
                dep: n.get(0)?.as_string()?.to_string(),
                name: text("name"),
                options,
                args,
                token: text("token"),
            }
        }
        // `virtualfile`/`hostfile` are the legacy names, still accepted on read.
        "volume" | "virtualfile" => SnapKind::Volume {
            name: n.get(0)?.as_string()?.to_string(),
            persist: ch
                .get("persist")
                .and_then(|p| p.get(0))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        },
        "bindmount" | "hostfile" => SnapKind::BindMount {
            path: PathBuf::from(n.get(0)?.as_string()?),
        },
        "note" => SnapKind::Note {
            text: text("text").unwrap_or_default(),
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
        "capture" => SnapKind::Capture,
        "clipboard" => SnapKind::Clipboard,
        "api" => SnapKind::Api,
        "midiin" => SnapKind::MidiIn {
            device: n
                .get(0)
                .and_then(|v| v.as_string())
                .unwrap_or("")
                .to_string(),
        },
        // The fabric name leads (like an app's dependency name); the host
        // target is a child so the line reads `hostservice "subduction" <id>`.
        "hostservice" => SnapKind::HostService {
            name: n.get(0)?.as_string()?.to_string(),
            target: text("target")?,
        },
        // `inport "<name>" "<connection kind>" "<id>"`. A malformed one never
        // reaches here: `validate_ports` rejects the whole document first, so
        // a typo in a boundary port is an error, not a vanished node.
        kw @ ("inport" | "outport") => {
            let name = n.get(0)?.as_string()?.to_string();
            let kind = PortKind::parse(n.get(1)?.as_string()?)?;
            if kw == "inport" {
                SnapKind::InPort { name, kind }
            } else {
                SnapKind::OutPort { name, kind }
            }
        }
        // `group "<definition name>" "<instance id>"`, with a boundary wire per
        // `in`/`out` child. Like a port, a malformed one never reaches here:
        // `validate_group` rejects the document first.
        "group" => {
            let mut in_wires = Vec::new();
            let mut out_wires = Vec::new();
            for c in ch.nodes() {
                let wires = match c.name().value() {
                    "in" => &mut in_wires,
                    "out" => &mut out_wires,
                    _ => continue,
                };
                let port = c.get(0)?.as_string()?.to_string();
                wires.push((port, c.get(1).and_then(node_id)?));
            }
            SnapKind::Group {
                definition: n.get(0)?.as_string()?.to_string(),
                name: text("name"),
                in_wires,
                out_wires,
            }
        }
        "network" => SnapKind::Net { gateway: false },
        "gateway" => SnapKind::Net { gateway: true },
        "router" => SnapKind::Router,
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
    let xy = |key: &str| -> Option<[f32; 2]> {
        let n = ch.get(key)?;
        Some([n.get(0).and_then(num)?, n.get(1).and_then(num)?])
    };
    // A declaration with no geometry takes the small-widget default. Nothing
    // else is that forgiving: a `note` without a `pos` is malformed, not a
    // note at the origin.
    let (pos, size) = match (xy("pos"), xy("size")) {
        (Some(pos), Some(size)) => (pos, size),
        _ if declaration => (
            xy("pos").unwrap_or([0.0, 0.0]),
            xy("size").unwrap_or([FILE_W, FILE_H]),
        ),
        _ => return None,
    };
    let pos3d = ch.get("pos3d").and_then(|p| {
        Some([
            p.get(0).and_then(num)?,
            p.get(1).and_then(num)?,
            p.get(2).and_then(num)?,
            p.get(3).and_then(num).unwrap_or(0.0),
        ])
    });
    // Absent means shown: hiding the panel is the exception a file records.
    let panel3d = ch
        .get("panel3d")
        .and_then(|p| p.get(0))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    Some(NodeSnap {
        id,
        pos,
        size,
        pos3d,
        panel3d,
        kind,
    })
}

/// Serialize one placed node under its kind's KDL node name.
fn snap_kdl(s: &NodeSnap) -> KdlNode {
    let name = match &s.kind {
        SnapKind::App { .. } => "node",
        SnapKind::Volume { .. } => "volume",
        SnapKind::BindMount { .. } => "bindmount",
        SnapKind::Port { .. } => "hostport",
        SnapKind::Net { gateway: false } => "network",
        SnapKind::Net { gateway: true } => "gateway",
        SnapKind::Router => "router",
        SnapKind::Iroh { .. } => "iroh",
        SnapKind::Veilid { .. } => "veilid",
        SnapKind::Note { .. } => "note",
        SnapKind::Capture => "capture",
        SnapKind::Clipboard => "clipboard",
        SnapKind::Api => "api",
        SnapKind::MidiIn { .. } => "midiin",
        SnapKind::HostService { .. } => "hostservice",
        SnapKind::InPort { .. } => "inport",
        SnapKind::OutPort { .. } => "outport",
        SnapKind::Group { .. } => "group",
    };
    let mut node = KdlNode::new(name);
    // Named kinds lead with the name (or note text), then the id.
    match &s.kind {
        SnapKind::App { dep: name, .. } | SnapKind::Volume { name, .. } => {
            node.push(str_entry(name));
        }
        SnapKind::BindMount { path } => {
            node.push(str_entry(&path.to_string_lossy()));
        }
        SnapKind::MidiIn { device } => {
            node.push(str_entry(device));
        }
        SnapKind::HostService { name, .. } => {
            node.push(str_entry(name));
        }
        // A boundary port reads as what it is: `inport "notes" "midi" "<id>"`.
        SnapKind::InPort { name, kind } | SnapKind::OutPort { name, kind } => {
            node.push(str_entry(name));
            node.push(str_entry(kind.as_str()));
        }
        // An instance names the definition it stamps out, then its own id.
        SnapKind::Group { definition, .. } => {
            node.push(str_entry(definition));
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
    // A chosen name leads the block, for both kinds that can have one. Emitted
    // before the match rather than as an arm of it: a group also writes its
    // boundary wires there, and a first-wins arm would have silently dropped
    // them from every named instance.
    if let SnapKind::App {
        name: Some(name), ..
    }
    | SnapKind::Group {
        name: Some(name), ..
    } = &s.kind
    {
        child_str("name", name);
    }
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
        SnapKind::Note { text } => child_str("text", text),
        SnapKind::HostService { target, .. } => child_str("target", target),
        // The boundary wires lead the block, like every other kind's own
        // content, with the geometry after them.
        SnapKind::Group {
            in_wires,
            out_wires,
            ..
        } => {
            for (keyword, wires) in [("in", in_wires), ("out", out_wires)] {
                for (port, node) in wires {
                    let mut w = KdlNode::new(keyword);
                    w.push(str_entry(port));
                    w.push(KdlEntry::new(node.to_string()));
                    ch.nodes_mut().push(w);
                }
            }
        }
        // Only a persisted volume writes the flag; the default is ephemeral.
        SnapKind::Volume { persist: true, .. } => {
            let mut p = KdlNode::new("persist");
            p.push(KdlEntry::new(true));
            ch.nodes_mut().push(p);
        }
        _ => {}
    }
    ch.nodes_mut().push(node2("pos", s.pos[0], s.pos[1]));
    ch.nodes_mut().push(node2("size", s.size[0], s.size[1]));
    if let Some(p3) = s.pos3d {
        let mut n = KdlNode::new("pos3d");
        for v in p3 {
            n.push(KdlEntry::new(v as f64));
        }
        ch.nodes_mut().push(n);
    }
    // Only a hidden panel writes the flag; the default is shown.
    if !s.panel3d {
        let mut n = KdlNode::new("panel3d");
        n.push(KdlEntry::new(false));
        ch.nodes_mut().push(n);
    }
    if let SnapKind::App {
        options,
        args,
        token,
        ..
    } = &s.kind
    {
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
        if let Some(tok) = token {
            let mut t = KdlNode::new("token");
            t.push(str_entry(tok));
            ch.nodes_mut().push(t);
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

/// Add a plugin to the file as a dependency. `target` is a local `.wasm` path,
/// an `oci://<ref>` registry reference, or a `docker://<Dockerfile>` build; the
/// name is its file stem, the OCI repository's last segment, or the
/// Dockerfile's directory name. The source is pulled/built now to validate it.
pub fn add(target: String, path: &Path) -> Result<(), String> {
    let mut doc = Document::load(path)?;
    let source = Source::parse(&target);
    let name = match &source {
        Source::Path(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".to_string()),
        Source::Oci(reference) => crate::oci::name_for(reference),
        // The Dockerfile's directory names the image (plugins/vim/Dockerfile -> vim).
        Source::Dockerfile(p) => p
            .parent()
            .and_then(|d| d.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_string()),
        // A local image ref: the tag's repo segment (registry/myapp:1.0 -> myapp).
        Source::Image(reference) => reference
            .rsplit('/')
            .next()
            .unwrap_or(reference)
            .split(':')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("image")
            .to_string(),
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

/// The OCI references `wk pull` should refresh: every `oci://` dependency of
/// the document, or — given a target — that dependency's reference (by name),
/// falling back to reading the target itself as a reference (`oci://` prefix
/// optional).
fn refs_to_pull(doc: &Document, target: Option<&str>) -> Result<Vec<String>, String> {
    let Some(target) = target else {
        return Ok(doc
            .dependencies
            .iter()
            .filter_map(|d| match &d.source {
                Source::Oci(reference) => Some(reference.clone()),
                _ => None,
            })
            .collect());
    };
    if let Some(dep) = doc.dependencies.iter().find(|d| d.name == target) {
        return match &dep.source {
            Source::Oci(reference) => Ok(vec![reference.clone()]),
            other => Err(format!(
                "dependency {target:?} is not an OCI artifact (it is {})",
                other.to_kdl()
            )),
        };
    }
    let reference = target.strip_prefix("oci://").unwrap_or(target);
    if !reference.contains('/') {
        return Err(format!(
            "no dependency named {target:?}, and it doesn't look like an OCI \
             reference (expected registry/repo[:tag])"
        ));
    }
    Ok(vec![reference.to_string()])
}

/// Re-pull OCI dependencies from their registries (like `docker pull`): a moved
/// tag repoints the cache's ref index at the new content; unchanged content is
/// a no-op (the blob store dedups). `target` selects one dependency (by name)
/// or a bare reference; `None` refreshes every `oci://` dependency.
pub fn pull(target: Option<String>, path: &Path) -> Result<(), String> {
    let doc = Document::load_resolved(path).unwrap_or_else(|_| Document::empty());
    let refs = refs_to_pull(&doc, target.as_deref())?;
    if refs.is_empty() {
        println!("(no oci:// dependencies to pull)");
        return Ok(());
    }
    for reference in refs {
        let before = crate::oci::cached_artifact(&reference);
        println!("pulling {reference} ...");
        crate::oci::pull_into_cache(&reference)?;
        let after = crate::oci::cached_artifact(&reference);
        match (before, after) {
            (Some(a), Some(b)) if a == b => println!("{reference}: up to date"),
            (Some(_), Some(_)) => println!("{reference}: updated"),
            _ => println!("{reference}: pulled"),
        }
    }
    Ok(())
}

/// Publish a local plugin to an OCI registry as a Wasm OCI Artifact. `plugin` is
/// a dependency name (resolved to its local wasm) or a `.wasm` path; `reference`
/// is the target, e.g. `localhost:5000/triangle:1.0`.
pub fn publish(plugin: String, reference: String, path: &Path) -> Result<(), String> {
    let wasm = Document::load_resolved(path)
        .ok()
        .and_then(|d| d.dependencies.into_iter().find(|d| d.name == plugin))
        .map(|d| d.local_path())
        .unwrap_or_else(|| PathBuf::from(&plugin));
    let bytes = std::fs::read(&wasm).map_err(|e| format!("reading {}: {e}", wasm.display()))?;
    crate::oci::push(&reference, &bytes)?;
    println!("published {} -> oci://{reference}", wasm.display());
    Ok(())
}

/// Print the dependencies available to the file — its own plus any pulled in via
/// `import` (marked `(imported)`).
pub fn list(path: &Path) -> Result<(), String> {
    let doc = Document::load_resolved(path)?;
    if doc.dependencies.is_empty() {
        println!("(no dependencies; add one with `wk add <path>`)");
    }
    for dep in &doc.dependencies {
        let tag = if doc.imported_deps.contains(&dep.name) {
            "  (imported)"
        } else {
            ""
        };
        match &dep.description {
            Some(d) => println!("  {}  {}  — {d}{tag}", dep.name, dep.source.to_kdl()),
            None => println!("  {}  {}{tag}", dep.name, dep.source.to_kdl()),
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
    fn refs_to_pull_selects_by_dep_name_or_reference() {
        let mut doc = Document::empty();
        let dep = |name: &str, source: Source| Dependency {
            name: name.to_string(),
            source,
            args: Vec::new(),
            description: None,
        };
        doc.dependencies
            .push(dep("foo", Source::Oci("ghcr.io/org/foo:1.0".to_string())));
        doc.dependencies
            .push(dep("bar", Source::Oci("localhost:5001/bar:2".to_string())));
        doc.dependencies
            .push(dep("local", Source::Path(PathBuf::from("a.wasm"))));

        // No target: every oci:// dependency, local paths skipped.
        assert_eq!(
            refs_to_pull(&doc, None).unwrap(),
            vec!["ghcr.io/org/foo:1.0", "localhost:5001/bar:2"]
        );
        // A dependency name resolves to its reference.
        assert_eq!(
            refs_to_pull(&doc, Some("bar")).unwrap(),
            vec!["localhost:5001/bar:2"]
        );
        // A non-OCI dependency is a clear error.
        assert!(refs_to_pull(&doc, Some("local"))
            .unwrap_err()
            .contains("not an OCI"));
        // An unmatched target is read as a reference (oci:// prefix optional)...
        assert_eq!(
            refs_to_pull(&doc, Some("oci://ghcr.io/other/thing:3")).unwrap(),
            vec!["ghcr.io/other/thing:3"]
        );
        assert_eq!(
            refs_to_pull(&doc, Some("ghcr.io/other/thing:3")).unwrap(),
            vec!["ghcr.io/other/thing:3"]
        );
        // ...but only if it plausibly is one.
        assert!(refs_to_pull(&doc, Some("typo"))
            .unwrap_err()
            .contains("no dependency"));
    }

    #[test]
    fn pos3d_round_trips_through_kdl() {
        let ws = NodeId::from_u128(7);
        let id = NodeId::from_u128(8);
        let text = format!(
            "workspace \"{ws}\" {{\n  \
             note \"{id}\" {{ text \"hi\"; pos 1 2; size 30 40; pos3d 0.5 1.5 -2.0 0.7 }}\n}}"
        );
        let doc = Document::from_kdl(&text).expect("parses");
        let snap = &doc.workspaces[0].nodes[0];
        assert_eq!(snap.pos3d, Some([0.5, 1.5, -2.0, 0.7]));

        // Round-trip: serialize and parse again; both survive.
        let back = Document::from_kdl(&doc.to_kdl()).expect("round-trips");
        assert_eq!(
            back.workspaces[0].nodes[0].pos3d,
            Some([0.5, 1.5, -2.0, 0.7])
        );
        // A node without one stays without one.
        let plain = Document::from_kdl(&format!(
            "workspace \"{ws}\" {{\n  note \"{id}\" {{ text \"x\"; pos 0 0; size 10 10 }}\n}}"
        ))
        .expect("parses");
        assert_eq!(plain.workspaces[0].nodes[0].pos3d, None);
    }

    #[test]
    fn a_hidden_3d_panel_round_trips_and_defaults_to_shown() {
        let ws = NodeId::from_u128(11);
        let id = NodeId::from_u128(12);
        let doc = Document::from_kdl(&format!(
            "workspace \"{ws}\" {{\n  \
             node \"totem\" \"{id}\" {{ pos 1 2; size 30 40; panel3d #false }}\n}}"
        ))
        .expect("parses");
        assert!(!doc.workspaces[0].nodes[0].panel3d);

        // The flag survives a trip back out to the file...
        let text = doc.to_kdl();
        assert!(text.contains("panel3d #false"), "not written: {text}");
        let back = Document::from_kdl(&text).expect("round-trips");
        assert!(!back.workspaces[0].nodes[0].panel3d);

        // ...and a node that never asked keeps its panel, writing no flag.
        let plain = Document::from_kdl(&format!(
            "workspace \"{ws}\" {{\n  node \"totem\" \"{id}\" {{ pos 0 0; size 10 10 }}\n}}"
        ))
        .expect("parses");
        assert!(plain.workspaces[0].nodes[0].panel3d);
        assert!(!plain.to_kdl().contains("panel3d"));
    }

    #[test]
    fn a_workspace_name_round_trips_and_leads_the_block() {
        let ws = NodeId::from_u128(21);
        let id = NodeId::from_u128(22);
        let doc = Document::from_kdl(&format!(
            "workspace \"{ws}\" {{\n  \
             node \"synth\" \"{id}\" {{ pos 1 2; size 30 40 }}\n  name \"voice\"\n}}"
        ))
        .expect("parses");
        assert_eq!(doc.workspaces[0].name.as_deref(), Some("voice"));
        // The name is not mistaken for a placed node.
        assert_eq!(doc.workspaces[0].nodes.len(), 1);

        // Serialization puts it first, whatever position it was authored in —
        // the tab's name is what the block is about, so it reads before content.
        let text = doc.to_kdl();
        let body = text.split_once('{').expect("workspace block").1;
        assert!(
            body.trim_start().starts_with("name \"voice\""),
            "name leads the block:\n{text}"
        );
        assert_eq!(
            Document::from_kdl(&text).unwrap().workspaces[0]
                .name
                .as_deref(),
            Some("voice")
        );

        // Absent stays absent (and writes no line); blank stays blank. The two
        // are different states: only `None` means "never named".
        let plain = Document::from_kdl(&format!("workspace \"{ws}\" {{\n}}")).expect("parses");
        assert_eq!(plain.workspaces[0].name, None);
        assert!(!plain.to_kdl().contains("name"));
        let blank = Document::from_kdl(&format!("workspace \"{ws}\" {{\n  name \"\"\n}}")).unwrap();
        assert_eq!(blank.workspaces[0].name.as_deref(), Some(""));
        assert_eq!(
            Document::from_kdl(&blank.to_kdl()).unwrap().workspaces[0].name,
            Some(String::new())
        );
    }

    #[test]
    fn tab_false_round_trips_and_a_tab_is_the_default() {
        let ws = NodeId::from_u128(41);
        let id = NodeId::from_u128(42);
        let doc = Document::from_kdl(&format!(
            "workspace \"{ws}\" {{\n  \
             name \"voice\"\n  tab #false\n  \
             note \"{id}\" {{ text \"hi\"; pos 1 2; size 30 40 }}\n}}"
        ))
        .expect("parses");
        assert!(!doc.workspaces[0].tab);
        // The flag is not mistaken for a placed node, and sits with the name.
        assert_eq!(doc.workspaces[0].nodes.len(), 1);

        let text = doc.to_kdl();
        assert!(text.contains("tab #false"), "not written: {text}");
        let body = text.split_once('{').expect("workspace block").1;
        assert!(
            body.trim_start().starts_with("name \"voice\"\n"),
            "name still leads, tab follows:\n{text}"
        );
        assert!(!Document::from_kdl(&text).unwrap().workspaces[0].tab);

        // A workspace that never opted out is a tab, and writes no flag — every
        // `.wk` file written before definitions existed is one of these.
        let plain = Document::from_kdl(&format!("workspace \"{ws}\" {{\n}}")).expect("parses");
        assert!(plain.workspaces[0].tab);
        assert!(!plain.to_kdl().contains("tab"));
    }

    #[test]
    fn boundary_ports_round_trip_and_may_leave_their_geometry_out() {
        let ws = NodeId::from_u128(61);
        let (a, b) = (NodeId::from_u128(62), NodeId::from_u128(63));
        let doc = Document::from_kdl(&format!(
            "workspace \"{ws}\" {{\n  \
             name \"voice\"\n  tab #false\n  \
             inport \"notes\" \"midi\" \"{a}\" {{ pos 10 20; size 130 44 }}\n  \
             outport \"samples\" \"bind\" \"{b}\"\n}}"
        ))
        .expect("parses");
        let nodes = &doc.workspaces[0].nodes;
        assert_eq!(
            nodes[0].kind,
            SnapKind::InPort {
                name: "notes".into(),
                kind: PortKind::Midi,
            }
        );
        assert_eq!(nodes[0].pos, [10.0, 20.0]);
        // A port is a declaration first: written without a block it still
        // places, at the default the canvas would have given it anyway.
        assert_eq!(
            nodes[1].kind,
            SnapKind::OutPort {
                name: "samples".into(),
                kind: PortKind::Bind,
            }
        );
        assert_eq!(nodes[1].size, [FILE_W, FILE_H]);

        // The written form names the port and its kind before the id, and both
        // survive the trip back.
        let text = doc.to_kdl();
        assert!(
            text.contains(&format!("inport \"notes\" \"midi\" \"{a}\"")),
            "{text}"
        );
        let back = Document::from_kdl(&text).expect("round-trips");
        assert_eq!(back.workspaces[0].nodes, *nodes);
    }

    #[test]
    fn a_boundary_ports_comment_survives_a_save() {
        // `Document::save` grafts comments on by node identity, and a port's id
        // is its third argument — a line the identity function reads wrongly
        // loses the comment above it on the very first save.
        let path = std::env::temp_dir().join("wk-port-comment-test.wk");
        let ws = NodeId::from_u128(71);
        let p = NodeId::from_u128(72);
        let original = format!(
            "{MODELINE}\n\
             workspace \"{ws}\" {{\n    \
             tab #false\n    \
             // the notes the caller plays into this voice\n    \
             inport \"notes\" \"midi\" \"{p}\" {{ pos 0 0; size 130 44 }}\n\
             }}\n"
        );
        std::fs::write(&path, &original).unwrap();
        let doc = Document::from_kdl(&original).expect("parses");
        doc.save(&path).expect("saves");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("// the notes the caller plays into this voice"),
            "{text}"
        );
        // And a second save is byte-identical (no churn from the new syntax).
        Document::from_kdl(&text).unwrap().save(&path).unwrap();
        assert_eq!(text, std::fs::read_to_string(&path).unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_malformed_boundary_port_is_a_load_error_not_a_dropped_node() {
        let ws = NodeId::from_u128(81);
        let p = NodeId::from_u128(82);
        let q = NodeId::from_u128(83);
        let load = |body: String| {
            Document::from_kdl(&format!("workspace \"{ws}\" {{\n{body}\n}}")).map(|_| ())
        };
        let port = |kw: &str, name: &str, kind: &str, id: NodeId| {
            format!("  {kw} \"{name}\" \"{kind}\" \"{id}\" {{ pos 0 0; size 10 10 }}")
        };

        // An unreadable kind names the mistake and the alternatives, instead of
        // the node quietly not being there.
        let err = load(port("inport", "notes", "mid", p)).unwrap_err();
        assert!(err.contains("\"mid\"") && err.contains("midi"), "{err}");

        // v1 refuses the two "exactly one per node" relations, and says why.
        let err = load(port("inport", "web", "net", p)).unwrap_err();
        assert!(err.contains("exactly one network"), "{err}");
        let err = load(port("outport", "http", "serve", p)).unwrap_err();
        assert!(err.contains("HostPort"), "{err}");

        // Two ports of the same direction can't share a name — a wire from
        // outside picks a port by that name and would have no way to choose.
        let dup = format!(
            "{}\n{}",
            port("inport", "notes", "midi", p),
            port("inport", "notes", "bind", q)
        );
        let err = load(dup).unwrap_err();
        assert!(err.contains("notes"), "{err}");
        // ...but the same name on opposite edges is the normal way to say
        // "this passes through".
        let through = format!(
            "{}\n{}",
            port("inport", "notes", "midi", p),
            port("outport", "notes", "midi", q)
        );
        assert!(load(through).is_ok());
    }

    /// A *named* group keeps its boundary wires through a save. Both are
    /// children of the same block, and emitting the name as another arm of the
    /// match that writes the wires would have dropped every `in`/`out` line
    /// from any instance someone bothered to name — silently, in their file.
    #[test]
    fn a_named_group_keeps_its_boundary_wires() {
        let (ws, group, peer) = (
            NodeId::from_u128(401),
            NodeId::from_u128(402),
            NodeId::from_u128(403),
        );
        let text = format!(
            "workspace \"{ws}\" {{\n    \
               group \"voice\" \"{group}\" {{ name \"lead\"; in \"notes\" \"{peer}\" }}\n}}\n"
        );
        let doc = Document::from_kdl(&text).expect("parses");
        let back = Document::from_kdl(&doc.to_kdl()).expect("round-trips");
        let SnapKind::Group { name, in_wires, .. } = &back.workspaces[0].nodes[0].kind else {
            panic!("a group")
        };
        assert_eq!(name.as_deref(), Some("lead"));
        assert_eq!(in_wires, &vec![("notes".to_string(), peer)]);
    }

    #[test]
    fn a_group_round_trips_with_its_boundary_wires() {
        let ws = NodeId::from_u128(91);
        let inst = NodeId::from_u128(92);
        let src = NodeId::from_u128(93);
        let dst = NodeId::from_u128(94);
        let doc = Document::from_kdl(&format!(
            "workspace \"{ws}\" {{\n  \
             group \"voice\" \"{inst}\" {{ pos 10 20; size 200 120; \
             in \"notes\" \"{src}\"; out \"audio\" \"{dst}\" }}\n}}"
        ))
        .expect("parses");
        let node = &doc.workspaces[0].nodes[0];
        assert_eq!(node.id, inst, "the instance id is the second argument");
        assert_eq!(
            node.kind,
            SnapKind::Group {
                definition: "voice".into(),
                name: None,
                in_wires: vec![("notes".into(), src)],
                out_wires: vec![("audio".into(), dst)],
            }
        );
        assert_eq!(node.pos, [10.0, 20.0]);

        // The written form names the definition before the instance id, and
        // the boundary wires survive — they are the whole point of the block.
        let text = doc.to_kdl();
        assert!(
            text.contains(&format!("group \"voice\" \"{inst}\"")),
            "{text}"
        );
        assert!(text.contains(&format!("in \"notes\" \"{src}\"")), "{text}");
        let back = Document::from_kdl(&text).expect("round-trips");
        assert_eq!(back.workspaces[0].nodes, doc.workspaces[0].nodes);

        // Like a boundary port, a group is a declaration first: written with
        // no block at all it still places, at the default geometry.
        let bare = Document::from_kdl(&format!(
            "workspace \"{ws}\" {{\n  group \"voice\" \"{inst}\"\n}}"
        ))
        .expect("parses");
        assert_eq!(bare.workspaces[0].nodes[0].size, [FILE_W, FILE_H]);
    }

    #[test]
    fn a_malformed_group_is_a_load_error_not_a_dropped_instance() {
        // A group the parser can't read is an instance that silently isn't
        // there — the same failure a boundary port's validation exists to
        // prevent, one level up.
        let ws = NodeId::from_u128(95);
        let inst = NodeId::from_u128(96);
        let load = |body: String| {
            Document::from_kdl(&format!("workspace \"{ws}\" {{\n{body}\n}}")).map(|_| ())
        };
        let err = load("  group \"voice\" { pos 0 0; size 10 10 }".to_string()).unwrap_err();
        assert!(err.contains("instance id"), "{err}");
        // A boundary wire needs both a port name and the node it joins.
        let err = load(format!("  group \"voice\" \"{inst}\" {{ in \"notes\" }}")).unwrap_err();
        assert!(err.contains("notes"), "{err}");
        // The whole, well-formed thing loads.
        assert!(load(format!(
            "  group \"voice\" \"{inst}\" {{ in \"notes\" \"{ws}\" }}"
        ))
        .is_ok());
    }

    #[test]
    fn a_groups_comments_survive_a_save() {
        // `Document::save` grafts comments on by node identity: a group's id is
        // its *second* argument, and a boundary wire inside the block has no
        // id at all — both need an identity of their own or the notes above
        // them are gone on the first save.
        let path = std::env::temp_dir().join("wk-group-comment-test.wk");
        let ws = NodeId::from_u128(97);
        let inst = NodeId::from_u128(98);
        let src = NodeId::from_u128(99);
        let original = format!(
            "{MODELINE}\n\
             workspace \"{ws}\" {{\n    \
             // one of the eight voices\n    \
             group \"voice\" \"{inst}\" {{\n        \
             // the keyboard feeds this one\n        \
             in \"notes\" \"{src}\"\n        \
             pos 0 0\n        size 200 120\n    \
             }}\n\
             }}\n"
        );
        std::fs::write(&path, &original).unwrap();
        let doc = Document::from_kdl(&original).expect("parses");
        doc.save(&path).expect("saves");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("// one of the eight voices"), "{text}");
        assert!(text.contains("// the keyboard feeds this one"), "{text}");
        // And a second save is byte-identical (no churn from the new syntax).
        Document::from_kdl(&text).unwrap().save(&path).unwrap();
        assert_eq!(text, std::fs::read_to_string(&path).unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_document_of_only_definitions_still_resolves_to_one_tab() {
        // Nothing in a definitions-only file is openable, but the client draws
        // its tab list straight from the resolved document — so running one has
        // to yield a canvas, without rewriting what the author actually wrote.
        let dir = std::env::temp_dir().join("wk-definitions-only");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let defs = dir.join("defs.wk");
        let a = NodeId::from_u128(51);
        let b = NodeId::from_u128(52);
        std::fs::write(
            &defs,
            format!(
                "workspace \"{a}\" {{\n  name \"voice\"\n  tab #false\n}}\n\
                 workspace \"{b}\" {{\n  name \"delay\"\n  tab #false\n}}\n"
            ),
        )
        .unwrap();

        let doc = Document::load_resolved(&defs).expect("resolves");
        assert_eq!(doc.workspaces.len(), 3, "the two definitions plus a tab");
        assert!(!doc.workspaces[0].tab && !doc.workspaces[1].tab);
        assert!(doc.workspaces[2].tab, "a scratch tab to open");
        assert_eq!(doc.workspaces[2].name, None);
        // Both definitions kept exactly what they said.
        assert_eq!(doc.workspaces[0].name.as_deref(), Some("voice"));
        assert_eq!(doc.workspaces[1].name.as_deref(), Some("delay"));

        // A file that already has a tab gets no extra one.
        let mixed = dir.join("mixed.wk");
        let c = NodeId::from_u128(53);
        std::fs::write(
            &mixed,
            format!("import \"defs.wk\"\nworkspace \"{c}\" {{\n}}\n"),
        )
        .unwrap();
        let doc = Document::load_resolved(&mixed).expect("resolves");
        assert_eq!(
            doc.workspaces.len(),
            3,
            "one tab + two imported definitions"
        );
        assert_eq!(doc.workspaces.iter().filter(|w| w.tab).count(), 1);
        // A definition is never the "phantom empty workspace" an import drops:
        // it comes through with its `tab #false` intact.
        for id in [a, b] {
            let ws = doc
                .workspaces
                .iter()
                .find(|w| w.id == id)
                .expect("imported");
            assert!(!ws.tab);
            assert!(doc.imported_workspaces.contains(&id));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_named_workspace_survives_being_imported_and_keeps_its_comment() {
        // Two things a name must not fall foul of: `resolve_into` drops a
        // workspace it judges "empty" (a name alone used to count as empty, so
        // an imported definition would vanish), and `Document::save` grafts
        // comments by node identity (a `name` line without one loses the
        // comment above it on the first save).
        let dir = std::env::temp_dir().join("wk-named-ws-import");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("root.wk");
        let child = dir.join("child.wk");
        let named = NodeId::from_u128(31);
        let own = NodeId::from_u128(32);
        std::fs::write(
            &root,
            format!(
                "{MODELINE}\n\
                 workspace \"{named}\" {{\n    \
                 // what this tab is for\n    \
                 name \"voice\"\n}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &child,
            format!("import \"root.wk\"\nworkspace \"{own}\" {{\n}}\n"),
        )
        .unwrap();

        let doc = Document::load_resolved(&child).unwrap();
        let imported = doc
            .workspaces
            .iter()
            .find(|w| w.id == named)
            .expect("the named workspace came through the import");
        assert_eq!(imported.name.as_deref(), Some("voice"));
        assert!(doc.imported_workspaces.contains(&named));

        // Re-saving the imported file keeps the name's comment.
        let root_doc = Document::load(&root).unwrap();
        root_doc.save(&root).unwrap();
        let text = std::fs::read_to_string(&root).unwrap();
        assert!(text.contains("// what this tab is for"), "{text}");
        assert!(text.contains("name \"voice\""), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_merges_for_running_and_save_preserves_the_composition() {
        let dir = std::env::temp_dir().join("wk-import-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("root.wk");
        let child = dir.join("child.wk");
        // Root: deps only (no workspace). Child: imports root, adds its own dep
        // and an own workspace.
        std::fs::write(&root, "dependencies {\n  triangle \"a/tri.wasm\"\n}\n").unwrap();
        let ws = NodeId::from_u128(42);
        std::fs::write(
            &child,
            format!(
                "import \"root.wk\"\ndependencies {{\n  synth \"b/synth.wasm\"\n}}\nworkspace \"{ws}\" {{\n}}\n"
            ),
        )
        .unwrap();

        // Running view: both deps present; root's is marked imported; the only
        // workspace is the child's (root's auto-blank is skipped).
        let doc = Document::load_resolved(&child).unwrap();
        let names: Vec<&str> = doc.dependencies.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"synth") && names.contains(&"triangle"));
        assert!(doc.imported_deps.contains("triangle"));
        assert!(!doc.imported_deps.contains("synth"));
        assert_eq!(doc.workspaces.len(), 1);
        assert_eq!(doc.workspaces[0].id, ws);
        assert_eq!(doc.imports, vec!["root.wk".to_string()]);

        // Saving the resolved doc back preserves the import and does NOT inline
        // the imported dependency.
        doc.save(&child).unwrap();
        let text = std::fs::read_to_string(&child).unwrap();
        assert!(text.contains("import"), "import line preserved");
        assert!(text.contains("synth"), "own dep kept");
        assert!(!text.contains("triangle"), "imported dep not inlined");

        // The raw single-file view still owns only its own dep + the import.
        let raw = Document::load(&child).unwrap();
        assert_eq!(raw.imports, vec!["root.wk".to_string()]);
        assert_eq!(raw.dependencies.len(), 1);
        assert_eq!(raw.dependencies[0].name, "synth");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn home_world_example_resolves_with_a_world_node_and_poses() {
        let example = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../example/home.wk"
        ));
        let doc = Document::load_resolved(example).expect("home.wk resolves");
        // The home workspace is present with posed nodes and the piano→synth wire.
        let ws = doc
            .workspaces
            .iter()
            .find(|w| !w.nodes.is_empty())
            .expect("home workspace");
        assert_eq!(ws.name.as_deref(), Some("plaza"), "the tab is named");
        let posed = ws.nodes.iter().filter(|n| n.pos3d.is_some()).count();
        assert!(posed >= 4, "all home nodes carry 3D poses (got {posed})");
        assert_eq!(ws.midi.len(), 1);
        // The plaza itself is a node now: a `world` app with its panel off,
        // fed the .glb by a bind mount wired into it.
        let world = ws
            .nodes
            .iter()
            .find(|n| matches!(&n.kind, SnapKind::App { dep, .. } if dep == "world"))
            .expect("a world node");
        assert!(!world.panel3d, "the world is geometry, not a card");
        let glb = ws
            .nodes
            .iter()
            .find(|n| matches!(&n.kind, SnapKind::BindMount { path } if path.ends_with("home.glb")))
            .expect("home.glb bind mount");
        assert!(
            ws.connections
                .iter()
                .any(|c| c.0 == glb.id && c.1 == world.id),
            "the .glb is mounted into the world node"
        );
    }

    #[test]
    fn repo_example_resolves_against_the_root_deps() {
        // The shipped example imports the repo's deps-only root workspace.
        let example = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../example/live-coding.wk"
        ));
        let doc = Document::load_resolved(example).expect("example resolves");
        let names: Vec<&str> = doc.dependencies.iter().map(|d| d.name.as_str()).collect();
        // Deps come entirely from the import (root workspace.wk).
        for want in ["shader", "vim", "piano"] {
            assert!(names.contains(&want), "{want} available via import");
            assert!(doc.imported_deps.contains(want), "{want} marked imported");
        }
        // The example's own workspace (with the host file wired in) is present.
        assert_eq!(doc.workspaces.len(), 1);
        let ws = &doc.workspaces[0];
        assert!(ws.nodes.iter().any(
            |n| matches!(&n.kind, SnapKind::BindMount { path } if path.ends_with("shader.wgsl"))
        ));
        assert_eq!(ws.connections.len(), 2, "host file wired to shader + vim");
        assert_eq!(ws.midi.len(), 1, "piano wired to shader");
    }

    #[test]
    fn filesystems_example_wires_provider_mounts() {
        // The nodes-as-filesystems showcase: app→app connections (provider
        // mounts) ride the same relation as volume binds, each with its own
        // mount path, and every provider dep resolves via the root import.
        let example = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../example/filesystems.wk"
        ));
        let doc = Document::load_resolved(example).expect("filesystems.wk resolves");
        let names: Vec<&str> = doc.dependencies.iter().map(|d| d.name.as_str()).collect();
        for want in [
            "bash",
            "hellofuse",
            "passfs",
            "zipfs",
            "hellofs",
            "httpfs",
            "kilo",
            "python",
        ] {
            assert!(names.contains(&want), "{want} available via import");
        }
        let ws = doc
            .workspaces
            .iter()
            .find(|w| !w.nodes.is_empty())
            .expect("demo workspace");
        // Four feeds (zip→zipfs, volume→passfs, www→python, zipfs→passfs —
        // the last a provider chained inside another provider) + five
        // provider mounts into bash + one into kilo.
        assert_eq!(ws.connections.len(), 10);
        let paths: Vec<&str> = ws.mount_paths.values().map(|s| s.as_str()).collect();
        for want in [
            "/hellofuse",
            "/zip",
            "/peer",
            "/hellofs",
            "/web",
            "/zipview",
            "/app",
        ] {
            assert!(paths.contains(&want), "{want} mount path present");
        }
        // hellofs serves two consumers (bash and kilo).
        let hellofs = ws
            .nodes
            .iter()
            .find(|n| matches!(&n.kind, SnapKind::App { dep, .. } if dep == "hellofs"))
            .expect("hellofs node")
            .id;
        assert_eq!(
            ws.connections
                .iter()
                .filter(|&&(f, _)| f == hellofs)
                .count(),
            2,
            "one provider, two consumers"
        );
        // The http pair shares a fabric network.
        assert_eq!(ws.net_links.len(), 2);
        // hellofuse's file comes from its args (fuse_opt_parse).
        assert!(ws.nodes.iter().any(|n| matches!(
            &n.kind,
            SnapKind::App { dep, args, .. }
                if dep == "hellofuse" && args.iter().any(|a| a.starts_with("--name="))
        )));
        assert!(ws.nodes.iter().any(
            |n| matches!(&n.kind, SnapKind::BindMount { path } if path.ends_with("demo-archive.zip"))
        ));
        assert!(ws.nodes.iter().any(
            |n| matches!(&n.kind, SnapKind::BindMount { path } if path.ends_with("httpfs-www"))
        ));
    }

    #[test]
    fn browser_example_wires_netsurf_to_a_web_server() {
        // The browser showcase: NetSurf + a CPython webserver on a shared
        // fabric network, the browser launched pointed at the server by name.
        let example = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../example/browser.wk"
        ));
        let doc = Document::load_resolved(example).expect("browser.wk resolves");
        let names: Vec<&str> = doc.dependencies.iter().map(|d| d.name.as_str()).collect();
        for want in ["netsurf", "python"] {
            assert!(names.contains(&want), "{want} available via import");
        }
        let ws = doc
            .workspaces
            .iter()
            .find(|w| !w.nodes.is_empty())
            .expect("browser workspace");
        // netsurf's launch args point at the server node's *name* — `www`,
        // which the file chooses, because a node's type (`python`) is not an
        // address and several nodes could share it.
        assert!(ws.nodes.iter().any(|n| matches!(
            &n.kind,
            SnapKind::App { dep, args, .. }
                if dep == "netsurf" && args.iter().any(|a| a.starts_with("http://www:"))
        )));
        // Both peers join the same network; the www dir feeds the server.
        assert_eq!(ws.net_links.len(), 2);
        assert!(ws.nodes.iter().any(
            |n| matches!(&n.kind, SnapKind::BindMount { path } if path.ends_with("browser-www"))
        ));
        assert_eq!(ws.connections.len(), 1);
    }

    #[test]
    fn imports_are_cycle_safe() {
        let dir = std::env::temp_dir().join("wk-import-cycle");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wk");
        let b = dir.join("b.wk");
        // a imports b, b imports a — must not loop forever.
        std::fs::write(&a, "import \"b.wk\"\ndependencies {\n  aa \"a.wasm\"\n}\n").unwrap();
        std::fs::write(&b, "import \"a.wk\"\ndependencies {\n  bb \"b.wasm\"\n}\n").unwrap();
        let doc = Document::load_resolved(&a).unwrap();
        let names: Vec<&str> = doc.dependencies.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"aa") && names.contains(&"bb"));
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        match Source::parse("docker://plugins/vim/Dockerfile") {
            Source::Dockerfile(p) => assert_eq!(p, PathBuf::from("plugins/vim/Dockerfile")),
            other => panic!("expected dockerfile source, got {other:?}"),
        }
        assert_eq!(
            Source::Dockerfile("plugins/vim/Dockerfile".into()).to_kdl(),
            "docker://plugins/vim/Dockerfile"
        );
        match Source::parse("image://myapp:1.0") {
            Source::Image(r) => assert_eq!(r, "myapp:1.0"),
            other => panic!("expected image source, got {other:?}"),
        }
        assert_eq!(
            Source::Image("myapp:1.0".into()).to_kdl(),
            "image://myapp:1.0"
        );
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
            imports: Vec::new(),
            imported_deps: std::collections::HashSet::new(),
            imported_workspaces: std::collections::HashSet::new(),
            scratch_tab: None,
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
                    name: Some("voice".into()),
                    tab: true,
                    nodes: vec![
                        NodeSnap {
                            id: synth,
                            pos: [40.0, 56.0],
                            size: [360.0, 260.0],
                            pos3d: None,
                            panel3d: true,
                            kind: SnapKind::App {
                                dep: "synth".into(),
                                name: None,
                                options: vec![8.0, 0.6, 0.0, 1.0],
                                args: vec!["netserve".into(), "80".into()],
                                token: Some("c0ffee".into()),
                            },
                        },
                        NodeSnap {
                            id: chan,
                            pos: [200.0, 120.0],
                            size: [130.0, 44.0],
                            pos3d: None,
                            panel3d: true,
                            kind: SnapKind::Volume {
                                name: "chan".into(),
                                persist: true,
                            },
                        },
                        NodeSnap {
                            id: notes,
                            pos: [200.0, 200.0],
                            size: [130.0, 44.0],
                            pos3d: None,
                            panel3d: true,
                            kind: SnapKind::BindMount {
                                path: "notes.txt".into(),
                            },
                        },
                        NodeSnap {
                            id: port,
                            pos: [600.0, 100.0],
                            size: [130.0, 44.0],
                            pos3d: None,
                            panel3d: true,
                            kind: SnapKind::Port { port: 8080 },
                        },
                        NodeSnap {
                            id: net,
                            pos: [700.0, 100.0],
                            size: [130.0, 44.0],
                            pos3d: None,
                            panel3d: true,
                            kind: SnapKind::Net { gateway: false },
                        },
                        NodeSnap {
                            id: gw,
                            pos: [700.0, 200.0],
                            size: [130.0, 44.0],
                            pos3d: None,
                            panel3d: true,
                            kind: SnapKind::Net { gateway: true },
                        },
                        NodeSnap {
                            id: mdst,
                            pos: [800.0, 100.0],
                            size: [130.0, 44.0],
                            pos3d: None,
                            panel3d: true,
                            kind: SnapKind::Iroh {
                                secret: Some(secret_hex(&[7u8; 32])),
                                peer: Some("endpointabc123".into()),
                            },
                        },
                        NodeSnap {
                            id: msrc,
                            pos: [900.0, 100.0],
                            size: [130.0, 44.0],
                            pos3d: None,
                            panel3d: true,
                            kind: SnapKind::Veilid {
                                secret: Some("VLD0:pubkey:secretkey".into()),
                                peer: Some("VLD0:remoterecordkey".into()),
                            },
                        },
                    ],
                    connections: vec![(chan, synth)],
                    mount_paths: BTreeMap::from([((chan, synth), "/data/notes.txt".to_string())]),
                    midi: vec![(msrc, mdst)],
                    serves: vec![(synth, port)],
                    serve_ports: BTreeMap::from([((synth, port), 3000u16)]),
                    net_links: vec![(synth, net)],
                    capture_links: vec![(synth, chan)],
                    clipboard_links: Vec::new(),
                    api_links: vec![(synth, net)],
                },
                // A definition: it rides along in the same document but is not
                // a tab, so `tab #false` has to survive the round-trip too.
                Workspace {
                    id: wb,
                    tab: false,
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

    #[test]
    fn legacy_file_node_keywords_still_parse() {
        // `virtualfile`/`hostfile` were renamed to `volume`/`bindmount`; a
        // workspace saved before the rename must still load.
        let ws = NodeId::from_u128(1);
        let v = NodeId::from_u128(2);
        let h = NodeId::from_u128(3);
        let text = format!(
            "workspace \"{ws}\" {{\n    \
             virtualfile \"notes.txt\" \"{v}\" {{ pos 0 0; size 10 10 }}\n    \
             hostfile \"data/log.txt\" \"{h}\" {{ pos 0 0; size 10 10 }}\n}}"
        );
        let doc = Document::from_kdl(&text).expect("legacy keywords parse");
        let kinds: Vec<_> = doc.workspaces[0].nodes.iter().map(|n| &n.kind).collect();
        assert_eq!(
            kinds,
            vec![
                &SnapKind::Volume {
                    name: "notes.txt".into(),
                    persist: false,
                },
                &SnapKind::BindMount {
                    path: PathBuf::from("data/log.txt")
                },
            ]
        );
    }

    #[test]
    fn save_preserves_comments_and_is_idempotent() {
        let path = std::env::temp_dir().join("wk-comments-preserve-test.wk");
        let ws = NodeId::from_u128(1);
        let hp = NodeId::from_u128(2);
        // A documented workspace: a header comment and an inline note on a node.
        let original = format!(
            "{MODELINE}\n\
             // A demo. Run:\n\
             //   wk run x.wk\n\
             workspace \"{ws}\" {{\n    \
             name \"demo\"\n    \
             // the published port\n    \
             hostport \"{hp}\" {{ port 8080; pos 0 0; size 10 10 }}\n\
             }}\n"
        );
        std::fs::write(&path, &original).unwrap();

        let doc = Document::from_kdl(&original).expect("parses");
        doc.save(&path).expect("saves");
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.starts_with(MODELINE));
        assert!(first.contains("// A demo. Run:"), "header kept:\n{first}");
        assert!(first.contains("//   wk run x.wk"));
        assert!(
            first.contains("// the published port"),
            "node note kept:\n{first}"
        );
        assert!(first.contains("port 8080"));
        assert!(
            first.contains("name \"demo\""),
            "the tab's name kept:\n{first}"
        );

        // Idempotent: re-parsing and saving the same model is byte-identical —
        // an unchanged workspace produces no diff churn.
        let doc2 = Document::from_kdl(&first).expect("re-parses");
        doc2.save(&path).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, second, "a no-op save churned the file");
        let _ = std::fs::remove_file(&path);
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
                prop::option::of(
                    prop::collection::vec(any::<u8>(), 1..48).prop_map(|b| bytes_hex(&b))
                ),
            )
                .prop_map(|(name, options, args, token)| SnapKind::App {
                    dep: name,
                    // Identity is chosen by whoever creates a node, never
                    // generated: an arbitrary one here would assert nothing.
                    name: None,
                    options,
                    args,
                    token
                }),
            (value_str(), any::<bool>())
                .prop_map(|(name, persist)| SnapKind::Volume { name, persist }),
            value_str().prop_map(|p| SnapKind::BindMount {
                path: PathBuf::from(p)
            }),
            any::<u16>().prop_map(|port| SnapKind::Port { port }),
            any::<bool>().prop_map(|gateway| SnapKind::Net { gateway }),
            uplink_fields().prop_map(|(secret, peer)| SnapKind::Iroh { secret, peer }),
            uplink_fields().prop_map(|(secret, peer)| SnapKind::Veilid { secret, peer }),
            value_str().prop_map(|text| SnapKind::Note { text }),
            Just(SnapKind::Capture),
            Just(SnapKind::Clipboard),
            Just(SnapKind::Api),
            (value_str(), value_str())
                .prop_map(|(name, target)| SnapKind::HostService { name, target }),
            (value_str(), port_kind()).prop_map(|(name, kind)| SnapKind::InPort { name, kind }),
            (value_str(), port_kind()).prop_map(|(name, kind)| SnapKind::OutPort { name, kind }),
            (
                value_str(),
                prop::option::of(value_str()),
                prop::collection::vec(boundary_wire(), 0..3),
                prop::collection::vec(boundary_wire(), 0..3),
            )
                .prop_map(|(definition, name, in_wires, out_wires)| SnapKind::Group {
                    definition,
                    name,
                    in_wires,
                    out_wires,
                },),
        ]
    }

    /// One `in`/`out` line of a `group` block: a port name and the node on the
    /// parent canvas it joins.
    fn boundary_wire() -> impl Strategy<Value = (String, NodeId)> {
        (value_str(), any_node_id())
    }

    /// A connection kind a boundary port may declare. `net` and `serve` are
    /// excluded: they parse, but `validate_ports` refuses them at load, so a
    /// document containing one is deliberately not round-trippable.
    fn port_kind() -> impl Strategy<Value = PortKind> {
        prop::sample::select(vec![
            PortKind::Bind,
            PortKind::Midi,
            PortKind::Capture,
            PortKind::Clipboard,
            PortKind::Api,
        ])
    }

    /// Make every boundary port's name unique within its direction, which the
    /// format requires — the generator would otherwise produce documents that
    /// are legal to write and (correctly) refused on the way back in.
    fn uniquify_port_names(nodes: &mut [NodeSnap]) {
        for (i, n) in nodes.iter_mut().enumerate() {
            match &mut n.kind {
                SnapKind::InPort { name, .. } | SnapKind::OutPort { name, .. } => {
                    name.push_str(&i.to_string())
                }
                _ => {}
            }
        }
    }

    fn node_snap() -> impl Strategy<Value = NodeSnap> {
        (
            any_node_id(),
            coord(),
            coord(),
            coord(),
            coord(),
            any::<bool>(),
            snap_kind(),
        )
            .prop_map(|(id, px, py, sx, sy, panel3d, kind)| NodeSnap {
                id,
                pos: [px, py],
                size: [sx, sy],
                pos3d: None,
                panel3d,
                kind,
            })
    }

    fn pair() -> impl Strategy<Value = (NodeId, NodeId)> {
        (any_node_id(), any_node_id())
    }

    fn workspace_strat() -> impl Strategy<Value = Workspace> {
        (
            any_node_id(),
            // Includes the empty string, which must stay distinguishable from
            // an absent name across the round-trip.
            prop::option::of(value_str()),
            any::<bool>(),
            prop::collection::vec(node_snap(), 0..6),
            prop::collection::vec(pair(), 0..3),
            prop::collection::vec(pair(), 0..3),
            prop::collection::vec(pair(), 0..3),
            prop::collection::vec(pair(), 0..3),
        )
            .prop_map(
                |(id, name, tab, mut nodes, conns, midi, serves, netlinks)| {
                    uniquify_port_names(&mut nodes);
                    Workspace {
                        capture_links: netlinks.clone(),
                        clipboard_links: netlinks.clone(),
                        api_links: netlinks.clone(),
                        id,
                        name,
                        tab,
                        nodes,
                        // Give every generated bind an explicit mount path so the 3rd-arg
                        // round-trip is exercised across the whole document space.
                        mount_paths: conns
                            .iter()
                            .map(|&p| (p, "/mnt/data".to_string()))
                            .collect(),
                        // Likewise a container port on every generated serve.
                        serve_ports: serves.iter().map(|&p| (p, 3000u16)).collect(),
                        connections: conns,
                        midi,
                        serves,
                        net_links: netlinks,
                    }
                },
            )
    }

    fn document() -> impl Strategy<Value = Document> {
        (
            prop::collection::vec(dependency(), 0..3),
            // A document always has at least one workspace.
            prop::collection::vec(workspace_strat(), 1..3),
        )
            .prop_map(|(dependencies, workspaces)| Document {
                imports: Vec::new(),
                dependencies,
                workspaces,
                imported_deps: std::collections::HashSet::new(),
                imported_workspaces: std::collections::HashSet::new(),
                scratch_tab: None,
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
