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

use crate::workspace::{Document, NodeSnap, SnapKind, Workspace};
use std::collections::BTreeMap;
use wk_protocol::NodeId;

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
    /// The chain of definition names from the tab down to and including this
    /// one (`["chorus", "voice"]`) — how an error names where it is.
    pub path: Vec<String>,
    /// The definition's content with every id run through [`derive_id`]: nodes
    /// and wiring ready to be materialized.
    ///
    /// `content.id` is the id of the **tab** the instance ultimately sits in,
    /// not the instance's own: expansion is flat by design, so the nodes belong
    /// to a real workspace and no reconciler ever learns instances exist.
    /// `content.name`/`content.tab` carry nothing and are left at their
    /// defaults. Nested `group` nodes are *not* among `content.nodes`; each has
    /// its own [`Instance`] in the expansion.
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
        let site = site_label(ws);
        for (id, definition) in groups_in(ws) {
            ex.group(ws.id, &site, id, definition)?;
        }
    }
    Ok(ex.out)
}

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

/// The `group` nodes of a workspace, in file order, as (instance id, name).
fn groups_in(ws: &Workspace) -> impl Iterator<Item = (NodeId, &str)> {
    ws.nodes.iter().filter_map(|n| match &n.kind {
        SnapKind::Group { definition, .. } => Some((n.id, definition.as_str())),
        _ => None,
    })
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
        tab: NodeId,
        site: &str,
        instance: NodeId,
        definition: &'a str,
    ) -> Result<(), String> {
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
        let pairs = |links: &[(NodeId, NodeId)]| -> Vec<(NodeId, NodeId)> {
            links.iter().map(|&(a, b)| (id(a), id(b))).collect()
        };
        self.out.push(Instance {
            id: instance,
            definition: definition.to_string(),
            defined_in: def.id,
            path: self.path.clone(),
            content: Workspace {
                // The instance's nodes belong to the tab, so the live graph
                // stays exactly as flat as it is today.
                id: tab,
                // A nested `group` is not a node of this instance: it is
                // another instance, and it follows with its own derived ids.
                nodes: def
                    .nodes
                    .iter()
                    .filter(|n| !matches!(n.kind, SnapKind::Group { .. }))
                    .map(|n| NodeSnap {
                        id: id(n.id),
                        ..n.clone()
                    })
                    .collect(),
                mount_paths: def
                    .mount_paths
                    .iter()
                    .map(|(&(a, b), path)| ((id(a), id(b)), path.clone()))
                    .collect(),
                serve_ports: def
                    .serve_ports
                    .iter()
                    .map(|(&(a, b), &port)| ((id(a), id(b)), port))
                    .collect(),
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

        let inner_site = format!("definition {definition:?}");
        for (nested, name) in groups_in(def) {
            self.group(tab, &inner_site, id(nested), name)?;
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
                name: name.to_string(),
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
        // Every instance's nodes land in the tab, not in some new workspace:
        // the live graph is flat and the reconcilers never learn about groups.
        assert!(out.iter().all(|i| i.content.id == NodeId::from_u128(20)));
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
