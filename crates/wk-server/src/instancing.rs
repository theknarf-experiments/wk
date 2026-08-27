//! Workspace **instancing**: what a `group` node stands for.
//!
//! A workspace with `tab #false` is a *definition* — content that exists to be
//! used from elsewhere rather than opened — and a `group "voice" "<id>"` node in
//! another workspace is one *instance* of it. This module resolves the name,
//! derives the ids the instance's nodes take, and recurses into the groups the
//! definition itself contains.
//!
//! It is deliberately a pure function of a resolved [`Document`]: no runtime
//! state, no side effects, nothing materialized. That is what lets a server
//! check the whole instancing of a file *before* it starts anything, and refuse
//! outright rather than half-expand.
//!
//! ## Why the ids are derived rather than minted
//!
//! An instance's nodes take `H(instance id, the definition's inner id)`. Two
//! instances of one definition therefore get two disjoint sets of ids; the same
//! file gives the same ids on every run, so a persisted volume inside a
//! definition keeps its sidecar (`<file>.wk.volumes/<id>`) across restarts; and
//! nothing has to be written back to the file to remember any of it. Because
//! those sidecar paths are the ids, **changing `H` renames every one of them** —
//! [`tests::derived_ids_are_stable`] pins literal values so that can only happen
//! on purpose.

use crate::wiring::{self, NodeClass};
use crate::workspace::{Document, NodeSnap, SnapKind, Workspace};
use std::collections::{BTreeMap, HashMap};
use wk_protocol::{NodeId, PortDir, PortKind};

/// The domain separator mixed into every derived id. It exists so this hash can
/// never collide with another use of SHA-256 over node ids elsewhere in wk, and
/// it is versioned because a change to it is a change to every derived id.
const DERIVE_DOMAIN: &[u8] = b"wk/instance-node/v1";

/// How deep `group`s may nest. A cycle is caught exactly (with its path), so
/// this is only a backstop against a legal-but-absurd tree — and against the
/// exponential fan-out one implies, which would otherwise be discovered as a
/// hang at startup rather than an error.
pub const MAX_DEPTH: usize = 16;

/// How many instances one document may expand to, for the same reason: sixteen
/// levels of a definition that contains two groups is 65535 instances, and no
/// real file is anywhere near this.
pub const MAX_INSTANCES: usize = 4096;

/// The id a node of `inner` (a definition's own id for it) takes inside the
/// instance identified by `instance`.
///
/// Total in both arguments and collision-free in practice: any `u128` is a
/// valid [`NodeId`] whose text form round-trips, so a derived id is as good an
/// id as a minted one.
pub fn derive_id(instance: NodeId, inner: NodeId) -> NodeId {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(DERIVE_DOMAIN);
    h.update(instance.as_u128().to_be_bytes());
    h.update(inner.as_u128().to_be_bytes());
    let digest = h.finalize();
    // The leading 128 bits; the rest of the digest is discarded.
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    NodeId::from_u128(u128::from_be_bytes(bytes))
}

/// One expanded instance: everything a `group` node stands for.
#[derive(Clone, Debug, PartialEq)]
pub struct Instance {
    /// The instance's identity — the `group` node's own id when the group is
    /// written in a tab, itself a derived id when it is written inside another
    /// definition.
    pub id: NodeId,
    /// The definition's name, as the `group` line wrote it.
    pub definition: String,
    /// The id of the workspace that *is* that definition.
    pub defined_in: NodeId,
    /// The tab the whole tree ultimately sits in. An instance is deliberately
    /// *not* a tab (see the module docs on landmine 12), so this is what a
    /// client's tab bar, `wk ps` and a workspace teardown go by.
    pub tab: NodeId,
    /// The instance this one is written inside, or `None` when its `group` node
    /// is written directly in a tab.
    pub parent: Option<NodeId>,
    /// The chain of definition names from the tab down to and including this
    /// one (`["chorus", "voice"]`) — how an error names where it is.
    pub path: Vec<String>,
    /// The definition's content with every id run through [`derive_id`]: nodes
    /// and wiring ready to be materialized.
    ///
    /// `content.id` is the instance's own id, so a materialized node's
    /// `NodeRec.ws` says which instance it belongs to and nothing that iterates
    /// the document's *tabs* can reach it — which is what keeps derived nodes
    /// out of the `.wk` file and out of a tab's canvas.
    /// `content.name`/`content.tab` carry nothing and are left at their
    /// defaults. Nested `group` nodes are *not* among `content.nodes`; each has
    /// its own [`Instance`] in the expansion. Neither are the definition's
    /// boundary ports: a port is a marker, not a runtime thing, so the wires
    /// through it are already collapsed onto whatever the parent wired in.
    pub content: Workspace,
}

/// Resolve every `group` in the document's tabs, recursively.
///
/// The result is in document order, parents before their children, so applying
/// it in order never depends on something later. An error names the workspace
/// or definition the offending group is written in; nothing is returned
/// half-expanded.
pub fn expand(doc: &Document) -> Result<Vec<Instance>, String> {
    let mut ex = Expander {
        doc,
        defs: definitions(doc)?,
        path: Vec::new(),
        out: Vec::new(),
    };
    for ws in doc.workspaces.iter().filter(|w| w.tab) {
        let site = Site {
            tab: ws.id,
            label: site_label(ws),
            canvas: &ws.nodes,
        };
        for node in groups_in(ws) {
            // A group written in a tab wires to the tab's own nodes, so an
            // endpoint on its `in`/`out` lines is already the live id.
            ex.group(&site, node.id, None, node, &|id| vec![id])?;
        }
    }
    Ok(ex.out)
}

/// Where a group is written, as everything the expansion needs to know about
/// the place: the tab the whole tree ultimately belongs to, how an error names
/// the site, and the canvas a boundary wire's endpoints must be on.
struct Site<'a> {
    tab: NodeId,
    label: String,
    /// The workspace's own nodes. A boundary wire reaches *this* canvas and no
    /// other, so this is both the endpoint lookup and the typecheck's input.
    canvas: &'a [NodeSnap],
}

/// How an endpoint written on a `group`'s `in`/`out` line resolves to live node
/// ids. In a tab it is that node, verbatim. Inside a definition it is the
/// derived node — unless it is one of the *containing* instance's own boundary
/// ports, in which case it is whatever the level above wired into that port:
/// nothing (so the wire is dropped rather than left dangling against a marker),
/// or several nodes (so it fans out).
type Outer<'f> = &'f dyn Fn(NodeId) -> Vec<NodeId>;

/// The walk's state: the document's definitions, the chain of names from the
/// tab down to where the walk is now (which is also how a cycle is spotted),
/// and what has been expanded so far.
struct Expander<'a> {
    doc: &'a Document,
    defs: BTreeMap<&'a str, &'a Workspace>,
    path: Vec<String>,
    out: Vec<Instance>,
}

/// Index the document's definitions by name.
///
/// Only `tab #false` workspaces are definitions: a tab is a *root* instance
/// (its nodes keep the ids the file wrote), so instantiating one is not a thing
/// the model has. Two tabs may therefore share a name — that is just two tabs
/// with the same label — while two definitions may not, because a `group` picks
/// its definition by nothing else.
fn definitions(doc: &Document) -> Result<BTreeMap<&str, &Workspace>, String> {
    let mut defs: BTreeMap<&str, &Workspace> = BTreeMap::new();
    for ws in doc.workspaces.iter().filter(|w| !w.tab) {
        let Some(name) = ws.name.as_deref() else {
            continue; // an unnamed definition is unreachable, not ambiguous
        };
        if let Some(first) = defs.insert(name, ws) {
            return Err(format!(
                "two definitions are named {name:?} (workspaces {} and {}); a group picks \
                 its definition by name, so a definition's name must be unique",
                first.id, ws.id
            ));
        }
    }
    Ok(defs)
}

/// The `group` nodes of a workspace, in file order.
fn groups_in(ws: &Workspace) -> impl Iterator<Item = &NodeSnap> {
    ws.nodes
        .iter()
        .filter(|n| matches!(n.kind, SnapKind::Group { .. }))
}

/// A `group`'s boundary wiring as written: `(port name, endpoint)` per line.
type BoundaryWires = [(String, NodeId)];

/// A group node's parts, for a snap already known to be one.
fn group_parts(n: &NodeSnap) -> (&str, &BoundaryWires, &BoundaryWires) {
    match &n.kind {
        SnapKind::Group {
            definition,
            in_wires,
            out_wires,
        } => (definition, in_wires, out_wires),
        _ => unreachable!("only a group snap reaches here"),
    }
}

/// A boundary port's direction, name and connection kind, if this snap is one.
fn port_of(n: &NodeSnap) -> Option<(PortDir, &str, PortKind)> {
    match &n.kind {
        SnapKind::InPort { name, kind } => Some((PortDir::In, name, *kind)),
        SnapKind::OutPort { name, kind } => Some((PortDir::Out, name, *kind)),
        _ => None,
    }
}

/// The keyword an `in`/`out` boundary wire is written with.
fn dir_word(dir: PortDir) -> &'static str {
    match dir {
        PortDir::In => "in",
        PortDir::Out => "out",
    }
}

/// What a node written in a `.wk` file is, for the purpose of classifying the
/// wire a boundary line would make. The runtime's `Server::class_of` decides
/// the same thing from the live graph and the two must agree, or a file would
/// load only to have its wiring refused (or the reverse) once it ran.
///
/// Everything but an app node is decided by its keyword alone, which is why
/// this can be checked before anything starts. An app is [`NodeClass::Other`]
/// — whether it *imports* MIDI is not knowable until its component compiles,
/// so a `midi` port fed by an app that has no MIDI is a wire that simply never
/// forms, exactly as it would be on the canvas.
fn class_of(kind: &SnapKind) -> NodeClass {
    match kind {
        SnapKind::Volume { .. } | SnapKind::BindMount { .. } => NodeClass::File,
        SnapKind::Port { .. } => NodeClass::Port,
        SnapKind::Net { .. } => NodeClass::Net,
        SnapKind::Router => NodeClass::Router,
        SnapKind::Iroh { .. } | SnapKind::Veilid { .. } => NodeClass::Uplink,
        SnapKind::Capture => NodeClass::Capture,
        SnapKind::Clipboard => NodeClass::Clipboard,
        SnapKind::Api => NodeClass::Api,
        SnapKind::MidiIn { .. } => NodeClass::MidiSource,
        SnapKind::HostService { .. } => NodeClass::HostSvc,
        SnapKind::InPort { name: _, kind } => NodeClass::Boundary(PortDir::In, *kind),
        SnapKind::OutPort { name: _, kind } => NodeClass::Boundary(PortDir::Out, *kind),
        SnapKind::Group { .. } => NodeClass::Instance,
        SnapKind::App { .. } | SnapKind::Note { .. } => NodeClass::Other,
    }
}

/// The word a `.wk` file writes a node with — how an error names the thing at
/// the wrong end of a boundary wire.
fn kind_word(kind: &SnapKind) -> &'static str {
    match kind {
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
    }
}

/// How an error names where a group is written.
fn site_label(ws: &Workspace) -> String {
    match &ws.name {
        Some(name) => format!("workspace {name:?}"),
        None => format!("workspace {}", ws.id),
    }
}

impl<'a> Expander<'a> {
    /// Expand one group and, depth-first, every group its definition contains.
    fn group(
        &mut self,
        at: &Site<'a>,
        instance: NodeId,
        parent: Option<NodeId>,
        node: &'a NodeSnap,
        outer: Outer,
    ) -> Result<(), String> {
        let (definition, in_wires, out_wires) = group_parts(node);
        // Every error below names the place the group is written.
        let site = &at.label;
        // A definition that contains itself, however indirectly, has no
        // expansion at all — so say which loop, not just that there is one.
        if let Some(at) = self.path.iter().position(|d| d == definition) {
            let mut cycle: Vec<&str> = self.path[at..].iter().map(String::as_str).collect();
            cycle.push(definition);
            return Err(format!(
                "{site}: group {definition:?} contains itself ({}); a definition cannot \
                 instantiate itself, directly or through another definition",
                cycle.join(" -> ")
            ));
        }
        if self.path.len() >= MAX_DEPTH {
            return Err(format!(
                "{site}: group {definition:?} nests groups more than {MAX_DEPTH} deep ({}); \
                 a definition this deep is almost certainly a mistake",
                self.path.join(" -> ")
            ));
        }
        let Some(def) = self.defs.get(definition).copied() else {
            // A tab of that name is the likely near-miss, and a much better
            // error than "no such definition" when the workspace is right
            // there in the file.
            let is_tab = self
                .doc
                .workspaces
                .iter()
                .any(|w| w.tab && w.name.as_deref() == Some(definition));
            return Err(if is_tab {
                format!(
                    "{site}: group {definition:?} names a workspace that is a tab, not a \
                     definition; add `tab #false` to it so it can be instantiated"
                )
            } else {
                format!(
                    "{site}: group {definition:?} names no definition; a group's first \
                     argument is the `name` of a workspace with `tab #false`"
                )
            });
        };
        if self.out.len() >= MAX_INSTANCES {
            return Err(format!(
                "{site}: group {definition:?} expands past {MAX_INSTANCES} instances; a \
                 definition somewhere below it fans out further on every level"
            ));
        }

        self.path.push(definition.to_string());
        let id = |inner: NodeId| derive_id(instance, inner);

        // What the level above wired into each of this definition's boundary
        // ports, keyed by the port's derived id. A key with an empty list is a
        // port the parent left unwired: every wire crossing it is dropped,
        // because a port never becomes a live node for it to dangle from.
        let mut ports: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        let mut named: HashMap<(PortDir, &str), (NodeId, PortKind)> = HashMap::new();
        let mut port_of_id: HashMap<NodeId, (PortDir, &str)> = HashMap::new();
        for n in &def.nodes {
            if let Some((dir, name, kind)) = port_of(n) {
                let derived = id(n.id);
                named.insert((dir, name), (derived, kind));
                port_of_id.insert(derived, (dir, name));
                ports.insert(derived, Vec::new());
            }
        }

        // A one-per-source relation has room for exactly one destination per
        // node, so a wire whose source is a port does not *add* anything to
        // whatever the parent wires in: the collapse hands that node a second
        // grant, and `toggle_unique` resolves it by dropping the first. The
        // first is a link written on the parent's own canvas, and `save`
        // re-projects the live graph, so the displacement would quietly delete
        // a line from the user's file — and two instances of the definition
        // would take the grant from each other the same way. This is the same
        // objection that refuses `net` and `serve` as port kinds; here it is
        // the wire's direction rather than the port's kind that decides, since
        // a port on the *destination* side leaves an inner node as the source.
        for (word, links) in [
            ("serve", &def.serves),
            ("netlink", &def.net_links),
            ("capturelink", &def.capture_links),
            ("clipboardlink", &def.clipboard_links),
            ("apilink", &def.api_links),
        ] {
            for &(src, _) in links {
                if let Some(&(dir, name)) = port_of_id.get(&id(src)) {
                    return Err(format!(
                        "{site}: group {definition:?} cannot be instantiated: its {}port \
                         {name:?} is the source of a `{word}` wire, which wk does not support \
                         yet — a node holds exactly one of these at a time, so crossing the \
                         boundary here would take that connection away from whatever the \
                         parent wires into {name:?} rather than adding one",
                        dir_word(dir)
                    ));
                }
            }
        }
        for (dir, wires) in [(PortDir::In, in_wires), (PortDir::Out, out_wires)] {
            for (name, endpoint) in wires {
                let word = dir_word(dir);
                let Some(&(port, kind)) = named.get(&(dir, name.as_str())) else {
                    // A port of that name on the *other* edge is the likely
                    // mistake, and a much better error than "no such port"
                    // when the name is right there in the definition.
                    if named.contains_key(&(dir.opposite(), name.as_str())) {
                        let other = dir_word(dir.opposite());
                        return Err(format!(
                            "{site}: group {definition:?} wires `{word} {name:?}`, but {name:?} is \
                             an {other}port of definition {definition:?}, not an {word}port; a \
                             boundary wire's direction is the port's, so write it as `{other}`"
                        ));
                    }
                    return Err(format!(
                        "{site}: group {definition:?} wires `{word} {name:?}`, but definition \
                         {definition:?} declares no {word}-port called {name:?}; a boundary \
                         wire names one of the definition's own `{word}port` lines"
                    ));
                };
                // The far end has to be a node of the canvas the group is
                // written on. Anywhere else and the wire would either dangle
                // or reach across a tab boundary, which nothing else in a
                // `.wk` file can do.
                let Some(far) = at.canvas.iter().find(|n| n.id == *endpoint) else {
                    return Err(format!(
                        "{site}: group {definition:?} wires `{word} {name:?} \"{endpoint}\"`, but \
                         no node with that id is on this canvas; a boundary wire joins one of \
                         the group's own neighbours to the port"
                    ));
                };
                // ...and it has to be a node the port's kind of connection can
                // reach. Seen from the parent, an instance's in-port is a
                // *consumer* — the same end of a wire the parent's own
                // out-port would play — so the typecheck is the ordinary one
                // against the complementary port. This is where a `midi` port
                // fed by a HostPort, or wired to another instance (whose own
                // ports the line cannot name), is stopped.
                let want = NodeClass::Boundary(dir.opposite(), kind);
                if wiring::classify(*endpoint, port, class_of(&far.kind), want).is_none() {
                    return Err(format!(
                        "{site}: group {definition:?} wires `{word} {name:?}` to a {} node, which \
                         cannot be the {} end of a `{}` connection; {name:?} is a `{}` {word}port \
                         of definition {definition:?}",
                        kind_word(&far.kind),
                        match dir {
                            PortDir::In => "source",
                            PortDir::Out => "destination",
                        },
                        kind.as_str(),
                        kind.as_str(),
                    ));
                }
                // Fan-in is just the same port named twice, so the bindings
                // accumulate rather than replace.
                ports.entry(port).or_default().extend(outer(*endpoint));
            }
        }

        // One id inside the definition, as live ids: the derived node, or —
        // when it is a boundary port — the nodes the parent bound it to. This
        // is the whole collapse: `X -> port` outside plus `port -> Y` inside
        // become a plain `X -> Y`, with no port left in the live graph. A
        // port wired straight to the complementary port passes through for
        // free, since both of its ends substitute.
        let sub = |inner: NodeId| -> Vec<NodeId> {
            let derived = id(inner);
            match ports.get(&derived) {
                Some(bound) => bound.clone(),
                None => vec![derived],
            }
        };
        let pairs = |links: &[(NodeId, NodeId)]| -> Vec<(NodeId, NodeId)> {
            let mut out = Vec::new();
            for &(a, b) in links {
                for x in sub(a) {
                    for y in sub(b) {
                        out.push((x, y));
                    }
                }
            }
            out
        };
        // A per-wire override follows every wire its key expanded into.
        let keyed =
            |src: &BTreeMap<(NodeId, NodeId), String>| -> BTreeMap<(NodeId, NodeId), String> {
                let mut out = BTreeMap::new();
                for ((a, b), v) in src {
                    for pair in pairs(&[(*a, *b)]) {
                        out.insert(pair, v.clone());
                    }
                }
                out
            };
        let mut serve_ports: BTreeMap<(NodeId, NodeId), u16> = BTreeMap::new();
        for (&(a, b), &port) in &def.serve_ports {
            for pair in pairs(&[(a, b)]) {
                serve_ports.insert(pair, port);
            }
        }
        self.out.push(Instance {
            id: instance,
            definition: definition.to_string(),
            defined_in: def.id,
            tab: at.tab,
            parent,
            path: self.path.clone(),
            content: Workspace {
                id: instance,
                // A nested `group` is not a node of this instance: it is
                // another instance, and it follows with its own derived ids.
                // A boundary port is not one either — the wiring through it
                // is already collapsed above.
                nodes: def
                    .nodes
                    .iter()
                    .filter(|n| !matches!(n.kind, SnapKind::Group { .. }))
                    .filter(|n| port_of(n).is_none())
                    .map(|n| NodeSnap {
                        id: id(n.id),
                        ..n.clone()
                    })
                    .collect(),
                mount_paths: keyed(&def.mount_paths),
                serve_ports,
                connections: pairs(&def.connections),
                midi: pairs(&def.midi),
                serves: pairs(&def.serves),
                net_links: pairs(&def.net_links),
                capture_links: pairs(&def.capture_links),
                clipboard_links: pairs(&def.clipboard_links),
                api_links: pairs(&def.api_links),
                ..Workspace::new()
            },
        });

        let inner = Site {
            tab: at.tab,
            label: format!("definition {definition:?}"),
            canvas: &def.nodes,
        };
        for nested in groups_in(def) {
            self.group(&inner, id(nested.id), Some(instance), nested, &sub)?;
        }
        self.path.pop();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::SnapKind;

    /// A workspace with the given name, `tab #false`, holding `nodes`.
    fn definition(id: u128, name: &str, nodes: Vec<NodeSnap>) -> Workspace {
        Workspace {
            id: NodeId::from_u128(id),
            name: Some(name.to_string()),
            tab: false,
            nodes,
            ..Workspace::new()
        }
    }

    fn tab(id: u128, nodes: Vec<NodeSnap>) -> Workspace {
        Workspace {
            id: NodeId::from_u128(id),
            nodes,
            ..Workspace::new()
        }
    }

    fn snap(id: u128, kind: SnapKind) -> NodeSnap {
        NodeSnap {
            id: NodeId::from_u128(id),
            pos: [0.0, 0.0],
            size: [130.0, 44.0],
            pos3d: None,
            panel3d: true,
            kind,
        }
    }

    fn app(id: u128, name: &str) -> NodeSnap {
        snap(
            id,
            SnapKind::App {
                dep: name.to_string(),
                name: None,
                options: Vec::new(),
                args: Vec::new(),
                token: None,
            },
        )
    }

    fn group(id: u128, definition: &str) -> NodeSnap {
        snap(
            id,
            SnapKind::Group {
                definition: definition.to_string(),
                in_wires: Vec::new(),
                out_wires: Vec::new(),
            },
        )
    }

    /// A group with boundary wiring: `(port name, endpoint id)` per direction.
    fn wired_group(
        id: u128,
        definition: &str,
        in_wires: &[(&str, u128)],
        out_wires: &[(&str, u128)],
    ) -> NodeSnap {
        let list = |w: &[(&str, u128)]| -> Vec<(String, NodeId)> {
            w.iter()
                .map(|&(n, e)| (n.to_string(), NodeId::from_u128(e)))
                .collect()
        };
        snap(
            id,
            SnapKind::Group {
                definition: definition.to_string(),
                in_wires: list(in_wires),
                out_wires: list(out_wires),
            },
        )
    }

    fn inport(id: u128, name: &str, kind: wk_protocol::PortKind) -> NodeSnap {
        snap(
            id,
            SnapKind::InPort {
                name: name.to_string(),
                kind,
            },
        )
    }

    fn outport(id: u128, name: &str, kind: wk_protocol::PortKind) -> NodeSnap {
        snap(
            id,
            SnapKind::OutPort {
                name: name.to_string(),
                kind,
            },
        )
    }

    fn doc(workspaces: Vec<Workspace>) -> Document {
        Document {
            workspaces,
            ..Document::empty()
        }
    }

    #[test]
    fn derived_ids_are_stable() {
        // These literals are the contract, not an implementation detail: the
        // derived id is where a persisted volume inside a definition keeps its
        // bytes (`<file>.wk.volumes/<id>`), so changing the hash silently
        // orphans every one of them. If this test fails, the change to
        // `derive_id` had better be deliberate.
        let instance = NodeId::from_u128(1);
        let inner = NodeId::from_u128(2);
        assert_eq!(
            derive_id(instance, inner).to_string(),
            "1TGT7N25PQ1QADZBRDGBSCSXF6"
        );
        // Not symmetric: which id is the instance and which the inner node is
        // part of what is hashed, or two instances could swap identities.
        assert_eq!(
            derive_id(inner, instance).to_string(),
            "2KVMJJW4YNHGZ0HF5VMGXEJ9N3"
        );
        // And a derived id is a real id: it round-trips through the text form
        // the `.wk` file and every wire message use.
        let derived = derive_id(instance, inner);
        assert_eq!(derived.to_string().parse::<NodeId>().unwrap(), derived);
    }

    #[test]
    fn each_instance_gets_its_own_disjoint_ids() {
        // The whole point of instancing: the same definition used twice is two
        // independent sets of nodes, and neither collides with the definition's
        // own ids (which stay authored content, never live).
        let d = doc(vec![
            definition(10, "voice", vec![app(11, "synth"), app(12, "reverb")]),
            tab(20, vec![group(21, "voice"), group(22, "voice")]),
        ]);
        let out = expand(&d).expect("expands");
        assert_eq!(out.len(), 2);
        let ids = |i: &Instance| -> Vec<NodeId> { i.content.nodes.iter().map(|n| n.id).collect() };
        let (a, b) = (ids(&out[0]), ids(&out[1]));
        assert_eq!(a.len(), 2);
        assert!(a.iter().all(|id| !b.contains(id)), "{a:?} vs {b:?}");
        for id in a.iter().chain(&b) {
            assert_ne!(*id, NodeId::from_u128(11));
            assert_ne!(*id, NodeId::from_u128(12));
        }
        // Every instance's nodes belong to the *instance*, and the instance
        // remembers which tab it is shown in. Keeping the two apart is what
        // stops a derived node reaching the `.wk` file or a tab's canvas.
        assert!(out.iter().all(|i| i.content.id == i.id));
        assert!(out.iter().all(|i| i.tab == NodeId::from_u128(20)));
        assert!(out.iter().all(|i| i.parent.is_none()));
        // And the definition's node kinds come through untouched — only ids
        // are rewritten.
        assert_eq!(out[0].content.nodes[0].kind, app(11, "synth").kind);
    }

    #[test]
    fn wiring_is_derived_with_the_nodes_it_joins() {
        // A definition's wires must follow its nodes into the instance, or an
        // expansion would place the nodes and lose everything between them.
        let (a, b) = (NodeId::from_u128(11), NodeId::from_u128(12));
        let inner = Workspace {
            connections: vec![(a, b)],
            mount_paths: BTreeMap::from([((a, b), "/data".to_string())]),
            midi: vec![(b, a)],
            ..definition(10, "voice", vec![app(11, "vol"), app(12, "synth")])
        };
        let d = doc(vec![inner, tab(20, vec![group(21, "voice")])]);
        let out = expand(&d).expect("expands");
        let instance = NodeId::from_u128(21);
        let (da, db) = (derive_id(instance, a), derive_id(instance, b));
        assert_eq!(out[0].content.connections, vec![(da, db)]);
        assert_eq!(out[0].content.midi, vec![(db, da)]);
        // Keyed side tables move with the wire they belong to.
        assert_eq!(
            out[0]
                .content
                .mount_paths
                .get(&(da, db))
                .map(String::as_str),
            Some("/data")
        );
    }

    #[test]
    fn nested_groups_expand_through_their_parent_instance() {
        // A definition may use other definitions. The inner instance's identity
        // has to come from the outer *instance*, not from the definition's own
        // group id, or two uses of the outer definition would share the inner
        // one's nodes.
        let d = doc(vec![
            definition(10, "voice", vec![app(11, "synth")]),
            definition(20, "chorus", vec![group(21, "voice"), group(22, "voice")]),
            tab(30, vec![group(31, "chorus"), group(32, "chorus")]),
        ]);
        let out = expand(&d).expect("expands");
        // Two chorus instances, each with two voices inside.
        assert_eq!(out.len(), 6);
        assert_eq!(
            out.iter().filter(|i| i.definition == "voice").count(),
            4,
            "each chorus brought its two voices"
        );
        // Parents come before their children, so the list can be applied in
        // order without a second pass.
        assert_eq!(out[0].definition, "chorus");
        assert_eq!(out[0].path, vec!["chorus"]);
        assert_eq!(out[1].path, vec!["chorus", "voice"]);
        // The nested instance's id is derived from the parent instance's.
        assert_eq!(
            out[1].id,
            derive_id(NodeId::from_u128(31), NodeId::from_u128(21))
        );
        // All four voices' synths are distinct nodes.
        let synths: std::collections::HashSet<NodeId> = out
            .iter()
            .filter(|i| i.definition == "voice")
            .map(|i| i.content.nodes[0].id)
            .collect();
        assert_eq!(synths.len(), 4);
        // A group node is never a node of its parent's content — it *is* the
        // child instance.
        assert!(out[0].content.nodes.is_empty());
    }

    #[test]
    fn a_boundary_port_collapses_into_the_wire_that_crosses_it() {
        use wk_protocol::PortKind;
        // The shape the whole feature is for: a definition whose in-port feeds
        // an inner app and whose out-port is fed by it. Expanded, the ports are
        // gone and the parent's own nodes are wired straight to the inner one —
        // a port is a marker on the file's canvas, never a runtime thing.
        let (port_in, port_out, synth) = (
            NodeId::from_u128(11),
            NodeId::from_u128(12),
            NodeId::from_u128(13),
        );
        let def = Workspace {
            midi: vec![(port_in, synth), (synth, port_out)],
            ..definition(
                10,
                "voice",
                vec![
                    inport(11, "notes", PortKind::Midi),
                    outport(12, "audio", PortKind::Midi),
                    app(13, "synth"),
                ],
            )
        };
        let d = doc(vec![
            def,
            tab(
                20,
                vec![
                    app(21, "keyboard"),
                    app(22, "speakers"),
                    wired_group(23, "voice", &[("notes", 21)], &[("audio", 22)]),
                ],
            ),
        ]);
        let out = expand(&d).expect("expands");
        let inst = &out[0];
        let derived = derive_id(NodeId::from_u128(23), synth);
        assert_eq!(
            inst.content.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![derived],
            "only the inner app materializes; the ports do not"
        );
        assert_eq!(
            inst.content.midi,
            vec![
                (NodeId::from_u128(21), derived),
                (derived, NodeId::from_u128(22)),
            ],
            "the parent's nodes are wired straight to the instance's"
        );
    }

    #[test]
    fn an_unwired_port_drops_the_wires_that_cross_it_rather_than_dangling() {
        use wk_protocol::PortKind;
        // A group may leave a port unwired — the definition still runs, just
        // without whatever was going to arrive. The inner wire has no source
        // then, and a port never becomes a node, so leaving it in would point
        // the live graph at an id nothing can resolve.
        let def = Workspace {
            midi: vec![(NodeId::from_u128(11), NodeId::from_u128(12))],
            ..definition(
                10,
                "voice",
                vec![inport(11, "notes", PortKind::Midi), app(12, "synth")],
            )
        };
        let d = doc(vec![def, tab(20, vec![group(21, "voice")])]);
        let out = expand(&d).expect("expands");
        assert!(out[0].content.midi.is_empty());
        assert_eq!(out[0].content.nodes.len(), 1, "the synth still runs");
    }

    #[test]
    fn one_port_wired_twice_fans_the_wire_out() {
        use wk_protocol::PortKind;
        // Two `in "notes"` lines are two sources feeding the same edge. The
        // collapse has to produce both wires — taking the last one would
        // silently drop a connection the author wrote.
        let def = Workspace {
            midi: vec![(NodeId::from_u128(11), NodeId::from_u128(12))],
            ..definition(
                10,
                "voice",
                vec![inport(11, "notes", PortKind::Midi), app(12, "synth")],
            )
        };
        let d = doc(vec![
            def,
            tab(
                20,
                vec![
                    app(21, "keys"),
                    app(22, "seq"),
                    wired_group(23, "voice", &[("notes", 21), ("notes", 22)], &[]),
                ],
            ),
        ]);
        let out = expand(&d).expect("expands");
        let synth = derive_id(NodeId::from_u128(23), NodeId::from_u128(12));
        assert_eq!(
            out[0].content.midi,
            vec![
                (NodeId::from_u128(21), synth),
                (NodeId::from_u128(22), synth),
            ]
        );
    }

    #[test]
    fn a_port_of_a_nested_definition_reaches_through_to_the_tab() {
        use wk_protocol::PortKind;
        // A definition may pass a connection straight through to one it uses.
        // The chain is `keyboard -> chorus:notes -> voice:notes -> synth`, and
        // with both ports collapsed the live wire has to be the two ends of it.
        let voice = Workspace {
            midi: vec![(NodeId::from_u128(11), NodeId::from_u128(12))],
            ..definition(
                10,
                "voice",
                vec![inport(11, "notes", PortKind::Midi), app(12, "synth")],
            )
        };
        let chorus = definition(
            20,
            "chorus",
            vec![
                inport(21, "notes", PortKind::Midi),
                // The nested group's endpoint is the *outer* definition's own
                // port, which is where the resolution has to keep going.
                wired_group(22, "voice", &[("notes", 21)], &[]),
            ],
        );
        let d = doc(vec![
            voice,
            chorus,
            tab(
                30,
                vec![
                    app(31, "keyboard"),
                    wired_group(32, "chorus", &[("notes", 31)], &[]),
                ],
            ),
        ]);
        let out = expand(&d).expect("expands");
        let inner = out
            .iter()
            .find(|i| i.definition == "voice")
            .expect("the nested voice");
        let synth = derive_id(inner.id, NodeId::from_u128(12));
        assert_eq!(inner.content.midi, vec![(NodeId::from_u128(31), synth)]);
        assert_eq!(inner.parent, Some(NodeId::from_u128(32)));
        assert_eq!(inner.tab, NodeId::from_u128(30));
    }

    #[test]
    fn a_boundary_wire_naming_no_port_is_an_error() {
        use wk_protocol::PortKind;
        // The definition's ports are the whole contract of a group, so a wire
        // naming one that isn't there is a mistake worth stopping for — the
        // alternative is a connection the author drew that silently isn't made.
        let def = definition(10, "voice", vec![inport(11, "notes", PortKind::Midi)]);
        let d = doc(vec![
            def,
            tab(
                20,
                vec![
                    app(21, "keys"),
                    wired_group(22, "voice", &[("note", 21)], &[]),
                ],
            ),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(err.contains("\"note\"") && err.contains("inport"), "{err}");

        // Names are per direction: `out "notes"` is not the in-port of that name.
        let def = definition(10, "voice", vec![inport(11, "notes", PortKind::Midi)]);
        let d = doc(vec![
            def,
            tab(
                20,
                vec![
                    app(21, "keys"),
                    wired_group(22, "voice", &[], &[("notes", 21)]),
                ],
            ),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(err.contains("outport"), "{err}");
    }

    #[test]
    fn a_boundary_wire_written_on_the_wrong_edge_says_which_edge_the_port_is_on() {
        use wk_protocol::PortKind;
        // The near-miss worth spelling out: the port exists and the name is
        // spelled right, it is just the other edge of the definition. "no
        // outport called notes" would send the author looking for a typo.
        let def = definition(10, "voice", vec![outport(11, "audio", PortKind::Midi)]);
        let d = doc(vec![
            def,
            tab(
                20,
                vec![
                    app(21, "speakers"),
                    wired_group(22, "voice", &[("audio", 21)], &[]),
                ],
            ),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(
            err.contains("is an outport") && err.contains("write it as `out`"),
            "{err}"
        );
    }

    #[test]
    fn a_boundary_wire_to_a_node_that_cannot_carry_the_connection_is_refused() {
        use wk_protocol::PortKind;
        // A port declares what may cross it, so the node on the other end has
        // to be able to play that end of that kind of wire. Without this the
        // collapse produces a live wire nothing classifies, which is silently
        // never made — the author's line does nothing and says nothing.
        let def = Workspace {
            midi: vec![(NodeId::from_u128(11), NodeId::from_u128(12))],
            ..definition(
                10,
                "voice",
                vec![inport(11, "notes", PortKind::Midi), app(12, "synth")],
            )
        };
        let hostport = snap(21, SnapKind::Port { port: 8080 });
        let d = doc(vec![
            def.clone(),
            tab(
                20,
                vec![hostport, wired_group(22, "voice", &[("notes", 21)], &[])],
            ),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(
            err.contains("hostport") && err.contains("source") && err.contains("midi"),
            "{err}"
        );

        // Wiring one instance to another is the same refusal, and the reason
        // is worth keeping: a boundary wire names one port and one node, so it
        // has no way to say *which* port of the far group it meant.
        let d = doc(vec![
            def,
            tab(
                20,
                vec![
                    group(21, "voice"),
                    wired_group(22, "voice", &[("notes", 21)], &[]),
                ],
            ),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(err.contains("group"), "{err}");
    }

    #[test]
    fn a_boundary_wire_to_a_node_that_is_not_on_this_canvas_is_refused() {
        use wk_protocol::PortKind;
        // A boundary wire joins the group to one of its own neighbours. An id
        // from somewhere else — another tab, a node since deleted — would
        // otherwise expand into a wire that either dangles or reaches across a
        // tab boundary, which nothing else in a `.wk` file can do.
        let def = Workspace {
            midi: vec![(NodeId::from_u128(11), NodeId::from_u128(12))],
            ..definition(
                10,
                "voice",
                vec![inport(11, "notes", PortKind::Midi), app(12, "synth")],
            )
        };
        let d = doc(vec![
            def,
            tab(20, vec![wired_group(22, "voice", &[("notes", 21)], &[])]),
            tab(30, vec![app(21, "keys")]),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(
            err.contains(&NodeId::from_u128(21).to_string()) && err.contains("this canvas"),
            "{err}"
        );
    }

    #[test]
    fn a_port_feeding_a_one_per_source_relation_is_refused() {
        use wk_protocol::PortKind;
        // `capturelink` and its two siblings are one grant per app. A port on
        // the *source* side means the parent's own node is the app, so the
        // collapse would move that node's grant onto the instance's capability
        // node — dropping a wire the parent's canvas holds, which `save` then
        // erases from the file.
        let def = Workspace {
            capture_links: vec![(NodeId::from_u128(11), NodeId::from_u128(12))],
            ..definition(
                10,
                "viewer",
                vec![
                    inport(11, "screen", PortKind::Capture),
                    app(12, "capture-node"),
                ],
            )
        };
        let d = doc(vec![
            def,
            tab(
                20,
                vec![
                    app(21, "app"),
                    wired_group(22, "viewer", &[("screen", 21)], &[]),
                ],
            ),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(
            err.contains("\"screen\"") && err.contains("capturelink"),
            "{err}"
        );

        // The mirror shape is the useful one and stays legal: an inner node
        // granted the capability the parent wires in. Its source is derived, so
        // it can displace nothing outside the instance. The parent's end has to
        // be a Capture *node* — that is what a `capture` in-port stands in for,
        // and an app there could never grant anything.
        let def = Workspace {
            capture_links: vec![(NodeId::from_u128(12), NodeId::from_u128(11))],
            ..definition(
                10,
                "viewer",
                vec![inport(11, "screen", PortKind::Capture), app(12, "eye")],
            )
        };
        let d = doc(vec![
            def,
            tab(
                20,
                vec![
                    snap(21, SnapKind::Capture),
                    wired_group(22, "viewer", &[("screen", 21)], &[]),
                ],
            ),
        ]);
        let out = expand(&d).expect("expands");
        let eye = derive_id(NodeId::from_u128(22), NodeId::from_u128(12));
        assert_eq!(
            out[0].content.capture_links,
            vec![(eye, NodeId::from_u128(21))]
        );
    }

    #[test]
    fn a_group_naming_no_definition_is_an_error_that_says_where() {
        let d = doc(vec![
            definition(10, "voice", vec![]),
            Workspace {
                name: Some("main".into()),
                ..tab(20, vec![group(21, "vioce")])
            },
        ]);
        let err = expand(&d).unwrap_err();
        assert!(err.contains("\"vioce\""), "{err}");
        assert!(err.contains("\"main\""), "names where it is: {err}");

        // A tab of that name is the near-miss worth calling out by itself: the
        // workspace exists, it just isn't instantiable.
        let d = doc(vec![
            Workspace {
                name: Some("voice".into()),
                ..tab(10, vec![])
            },
            tab(20, vec![group(21, "voice")]),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(err.contains("tab #false"), "{err}");
    }

    #[test]
    fn two_definitions_of_one_name_are_refused_naming_both() {
        // A group has nothing but the name to pick by, so the ambiguity is an
        // error even before anything uses it.
        let d = doc(vec![
            definition(10, "voice", vec![]),
            definition(11, "voice", vec![]),
            tab(20, vec![]),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(err.contains(&NodeId::from_u128(10).to_string()), "{err}");
        assert!(err.contains(&NodeId::from_u128(11).to_string()), "{err}");

        // Two *tabs* sharing a name are not definitions and stay legal — that
        // is just two tabs with the same label, which every file written before
        // definitions existed is free to have.
        let named = |id: u128| Workspace {
            name: Some("scratch".into()),
            ..tab(id, vec![])
        };
        assert!(expand(&doc(vec![named(30), named(31)])).is_ok());
    }

    #[test]
    fn a_definition_that_contains_itself_is_refused_with_the_cycle() {
        // Directly...
        let d = doc(vec![
            definition(10, "voice", vec![group(11, "voice")]),
            tab(20, vec![group(21, "voice")]),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(err.contains("voice -> voice"), "{err}");

        // ...and through another definition, where the path is the only way to
        // see which loop was found.
        let d = doc(vec![
            definition(10, "voice", vec![group(11, "chorus")]),
            definition(20, "chorus", vec![group(21, "voice")]),
            tab(30, vec![group(31, "voice")]),
        ]);
        let err = expand(&d).unwrap_err();
        assert!(err.contains("voice -> chorus -> voice"), "{err}");

        // A definition used twice as a *sibling* is not a cycle — that is the
        // ordinary case of reusing a building block.
        let d = doc(vec![
            definition(40, "voice", vec![]),
            definition(50, "chorus", vec![group(51, "voice"), group(52, "voice")]),
            tab(60, vec![group(61, "chorus"), group(62, "voice")]),
        ]);
        assert!(expand(&d).is_ok());
    }

    #[test]
    fn nesting_past_the_depth_cap_is_refused() {
        // A chain of definitions each using the next: legal, acyclic, and at
        // some depth no longer something anyone meant to write.
        let mut workspaces: Vec<Workspace> = (0..MAX_DEPTH + 2)
            .map(|i| {
                definition(
                    100 + i as u128,
                    &format!("d{i}"),
                    vec![group(200 + i as u128, &format!("d{}", i + 1))],
                )
            })
            .collect();
        workspaces.push(tab(1, vec![group(2, "d0")]));
        let err = expand(&doc(workspaces)).unwrap_err();
        assert!(err.contains(&MAX_DEPTH.to_string()), "{err}");
        // Just inside the cap still expands.
        let mut workspaces: Vec<Workspace> = (0..MAX_DEPTH - 1)
            .map(|i| {
                definition(
                    100 + i as u128,
                    &format!("d{i}"),
                    vec![group(200 + i as u128, &format!("d{}", i + 1))],
                )
            })
            .collect();
        workspaces.push(definition(99, &format!("d{}", MAX_DEPTH - 1), vec![]));
        workspaces.push(tab(1, vec![group(2, "d0")]));
        assert_eq!(expand(&doc(workspaces)).unwrap().len(), MAX_DEPTH);
    }

    #[test]
    fn a_fan_out_past_the_instance_cap_is_refused() {
        // The depth cap alone doesn't bound the *size* of an expansion: a
        // definition holding two groups doubles every level, so a dozen legal,
        // acyclic levels is thousands of instances. Without this the mistake
        // shows up as a server that never finishes starting.
        let mut workspaces: Vec<Workspace> = (0..13u128)
            .map(|i| {
                definition(
                    100 + i,
                    &format!("d{i}"),
                    vec![
                        group(200 + i * 2, &format!("d{}", i + 1)),
                        group(201 + i * 2, &format!("d{}", i + 1)),
                    ],
                )
            })
            .collect();
        workspaces.push(definition(99, "d13", vec![]));
        workspaces.push(tab(1, vec![group(2, "d0")]));
        let err = expand(&doc(workspaces)).unwrap_err();
        assert!(err.contains(&MAX_INSTANCES.to_string()), "{err}");
    }

    #[test]
    fn the_blocks_example_expands_to_two_voices_and_does_not_notice_being_split_in_two_files() {
        // The shipped example is the whole feature end to end: one definition,
        // two instances, one piano reaching both through their in-ports.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example");
        let inline = Document::load_resolved(&dir.join("blocks.wk")).expect("blocks.wk resolves");
        let out = expand(&inline).expect("blocks.wk expands");
        assert_eq!(out.len(), 2, "two voices");
        let piano = inline.workspaces[0]
            .nodes
            .iter()
            .find(|n| matches!(&n.kind, SnapKind::App { dep, .. } if dep == "piano"))
            .expect("the piano");
        for inst in &out {
            assert_eq!(inst.definition, "voice");
            let names: Vec<&str> = inst
                .content
                .nodes
                .iter()
                .map(|n| match &n.kind {
                    SnapKind::App { dep, .. } => dep.as_str(),
                    _ => "?",
                })
                .collect();
            assert_eq!(names, ["arp", "synth"], "the port did not materialize");
            let (arp, synth) = (inst.content.nodes[0].id, inst.content.nodes[1].id);
            // The piano is wired straight to *this* instance's arp: the
            // definition's `notes -> arp` and the group's `in "notes" piano`
            // have collapsed into one wire, with no port in between.
            assert_eq!(inst.content.midi, vec![(piano.id, arp), (arp, synth)]);
        }
        // Two instances, two disjoint sets of nodes — that is what makes them
        // independent voices rather than one voice drawn twice.
        assert_ne!(out[0].content.nodes[0].id, out[1].content.nodes[0].id);

        // The promise the feature is for: moving the definition into its own
        // file and importing it leaves the call site alone. Not "much the
        // same" — the same instances, node for node and wire for wire.
        let split = Document::load_resolved(&dir.join("blocks-imported.wk"))
            .expect("blocks-imported.wk resolves");
        assert_eq!(expand(&split).expect("expands"), out);
    }

    #[test]
    fn a_document_without_groups_expands_to_nothing() {
        // Every `.wk` file written so far is one of these, so this is the case
        // that must stay free: no groups, no work, no error.
        let d = doc(vec![
            tab(10, vec![app(11, "synth")]),
            definition(20, "voice", vec![app(21, "reverb")]),
        ]);
        assert!(expand(&d).unwrap().is_empty());
        assert!(expand(&Document::empty()).unwrap().is_empty());
    }

    #[test]
    fn a_group_inside_an_unused_definition_is_not_expanded_but_is_still_checked() {
        // Expansion starts at the tabs, so a definition nobody uses places no
        // nodes...
        let d = doc(vec![
            definition(10, "voice", vec![app(11, "synth")]),
            definition(20, "chorus", vec![group(21, "voice")]),
            tab(30, vec![]),
        ]);
        assert!(expand(&d).unwrap().is_empty());
        // ...but a duplicate name in one is still a hard error, because the
        // first `group` to be written would silently pick one of the two.
        let d = doc(vec![
            definition(10, "voice", vec![]),
            definition(11, "voice", vec![]),
            tab(30, vec![]),
        ]);
        assert!(expand(&d).is_err());
    }
}
