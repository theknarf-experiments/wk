//! Pure wiring logic, extracted from [`crate::server::Server`] so it can be
//! reasoned about — and property-tested — without a live runtime.
//!
//! The server owns the *effects* of wiring (mounting files into a guest's fs,
//! starting an HTTP server, routing MIDI, joining a fabric network). This module
//! owns the *decisions*: which kind of wire two nodes form, how toggling a link
//! updates the link set, and which servers must start or stop to match the
//! desired serve wiring. All functions here are pure — no I/O, no locks, no
//! `Server` — so they are cheap to test exhaustively.

use std::collections::{HashMap, HashSet};
use wk_protocol::{NodeId, PortDir, PortKind, Wire};

/// What a node is, for the purpose of classifying a wire between two nodes. A
/// node is exactly one of these (file/port/net node sets are disjoint; anything
/// else — an app node, or a not-yet-known id — is [`NodeClass::Other`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeClass {
    File,
    Port,
    Net,
    /// An uplink node — wires only to a Network (the net it extends). Iroh and
    /// Veilid extend it to a remote fabric; a Multicast bridge extends its
    /// multicast domain to the host's real network. All three attach a trunk to
    /// exactly one Network, which is the whole of what wiring needs to know.
    Uplink,
    /// A HostService node — wires only to a Network (the net it publishes a
    /// host TCP service into, as a named fabric peer).
    HostSvc,
    /// A Screen Capture node — wires only to an app (granting it frames).
    Capture,
    /// A Clipboard node — wires only to an app (granting it the host's
    /// system clipboard).
    Clipboard,
    /// A wk API node — wires only to an app (granting it API access).
    Api,
    /// A hardware MIDI input node — a MIDI *source* only (wires to an app's MIDI
    /// input, never a destination).
    MidiSource,
    /// A hardware MIDI output node — a MIDI *destination* only. Whatever is
    /// wired into it is played out of a physical MIDI port, so the canvas can
    /// drive an external synth or drum machine.
    MidiSink,
    /// A workspace **boundary port**: a placed stand-in for a node on the other
    /// side of the workspace's edge. Unlike every other class it is *declared*
    /// rather than inferred, so it carries its own connection kind and the end
    /// of that connection it plays.
    Boundary(PortDir, PortKind),
    /// A router node: joins Networks like an app does, except it may join
    /// several — bridging them is the whole point of it.
    Router,
    /// A `group` node — an **instance** of another workspace. Its edges are the
    /// definition's boundary ports, which are wired in the file's `group` block
    /// by port name; nothing on the live canvas connects to the instance node
    /// itself, so it forms no wire with anything.
    Instance,
    Other,
}

/// Classify the wire that connecting `a`↔`b` would form, given each node's
/// class, and return it with its canonical orientation, or `None` if the pair
/// can't be wired.
///
/// A file/port/net node only wires to an *app* ([`NodeClass::Other`]): a File
/// mounts into the app, a HostPort serves the app (the http node), a Network is
/// joined by the app. Two apps form a MIDI link. An uplink node (Iroh, Veilid) joins a Network
/// exactly like an app does (it's a member whose "traffic" is the remote
/// fabric). Any other pairing — two special nodes, or the same special kind
/// twice — can't be wired.
pub fn classify(a: NodeId, b: NodeId, ca: NodeClass, cb: NodeClass) -> Option<Wire> {
    use NodeClass::*;
    // A boundary port is matched first and on its own terms: it declares
    // exactly one connection kind, so a partner that doesn't fit is refused
    // outright rather than falling through to a looser rule below (an app on
    // the far end of a `midi` port must not become a bind).
    if let Boundary(dir, kind) = ca {
        return boundary_wire(a, dir, kind, b, cb);
    }
    if let Boundary(dir, kind) = cb {
        return boundary_wire(b, dir, kind, a, ca);
    }
    match (ca, cb) {
        (File, Other) => Some(Wire::Bind(a, b)),
        (Other, File) => Some(Wire::Bind(b, a)),
        // The http node is the app side; the HostPort is the second element.
        (Port, Other) => Some(Wire::Serve(b, a)),
        (Other, Port) => Some(Wire::Serve(a, b)),
        // The app (or uplink, or host service) is the first element; the
        // network the second.
        (Net, Other) | (Net, Uplink) | (Net, HostSvc) | (Net, Router) => Some(Wire::Net(b, a)),
        (Other, Net) | (Uplink, Net) | (HostSvc, Net) | (Router, Net) => Some(Wire::Net(a, b)),
        // The app is the first element; the capture source the second.
        (Capture, Other) => Some(Wire::Capture(b, a)),
        (Other, Capture) => Some(Wire::Capture(a, b)),
        // The app is the first element; the Clipboard node the second.
        (Clipboard, Other) => Some(Wire::Clipboard(b, a)),
        (Other, Clipboard) => Some(Wire::Clipboard(a, b)),
        // The app is the first element; the API node the second.
        (Api, Other) => Some(Wire::Api(b, a)),
        (Other, Api) => Some(Wire::Api(a, b)),
        // A hardware MIDI source drives an app's MIDI input: the source is always
        // the first element of the MIDI link (it can't be a destination).
        (MidiSource, Other) => Some(Wire::Midi(a, b)),
        (Other, MidiSource) => Some(Wire::Midi(b, a)),
        // A hardware MIDI sink is the mirror image: always the link's
        // destination, never its source.
        (MidiSink, Other) => Some(Wire::Midi(b, a)),
        (Other, MidiSink) => Some(Wire::Midi(a, b)),
        // The two hardware ends wire to each other, which is a MIDI thru box:
        // a keyboard playing an external sound module, with wk in between.
        (MidiSource, MidiSink) => Some(Wire::Midi(a, b)),
        (MidiSink, MidiSource) => Some(Wire::Midi(b, a)),
        (Other, Other) => Some(Wire::Midi(a, b)),
        _ => None,
    }
}

/// The wire a boundary port forms with the node it is wired to, already in its
/// canonical orientation.
///
/// An **in**-port stands in for whatever the far side will supply — the volume
/// of a bind, the source of a MIDI link, the capability node of a grant — and
/// takes that end of the wire. An **out**-port stands in for the far side's
/// *consumer* and takes the other end. Expansion can then collapse an outer
/// `X -> inport` and an inner `inport -> Y` into a plain `X -> Y` of the same
/// kind, with no port left in the live graph.
///
/// The complementary port of the same kind is a legal partner too — an in-port
/// wired straight to an out-port passes a connection through a definition
/// without an inner node in the middle.
fn boundary_wire(
    port: NodeId,
    dir: PortDir,
    kind: PortKind,
    other: NodeId,
    oc: NodeClass,
) -> Option<Wire> {
    use NodeClass::*;
    use PortDir::{In, Out};
    // The far end fits if it is the class this port's own end joins to, or the
    // matching port of the same kind on the opposite edge.
    let fits = |want: NodeClass| oc == want || oc == Boundary(dir.opposite(), kind);
    match (dir, kind) {
        // An in-port supplies what an app consumes.
        (In, PortKind::Bind) => fits(Other).then_some(Wire::Bind(port, other)),
        (In, PortKind::Midi) => (fits(Other) || oc == MidiSink).then_some(Wire::Midi(port, other)),
        (In, PortKind::Capture) => fits(Other).then_some(Wire::Capture(other, port)),
        (In, PortKind::Clipboard) => fits(Other).then_some(Wire::Clipboard(other, port)),
        (In, PortKind::Api) => fits(Other).then_some(Wire::Api(other, port)),
        // An out-port consumes what an inner node provides. A bind's provider
        // is a file node or an app serving `wk:fs/provider` (which mounts into
        // things exactly like a volume); a MIDI source is an app or a hardware
        // input.
        (Out, PortKind::Bind) => (fits(File) || oc == Other).then_some(Wire::Bind(other, port)),
        (Out, PortKind::Midi) => {
            (fits(Other) || oc == MidiSource).then_some(Wire::Midi(other, port))
        }
        (Out, PortKind::Capture) => fits(Capture).then_some(Wire::Capture(port, other)),
        (Out, PortKind::Clipboard) => fits(Clipboard).then_some(Wire::Clipboard(port, other)),
        (Out, PortKind::Api) => fits(Api).then_some(Wire::Api(port, other)),
        // Refused at load (see `workspace::validate_ports`) — but classify is
        // total, and refusing the wire is the safe answer if one ever exists.
        (_, PortKind::Net | PortKind::Serve) => None,
    }
}

/// Toggle a plain `(a, b)` link: remove it if present, else append it. Returns
/// whether the link is present afterward (`true` = just connected). Used for
/// file and MIDI links, which have no "one per" constraint.
pub fn toggle_pair(links: &mut Vec<(NodeId, NodeId)>, a: NodeId, b: NodeId) -> bool {
    if let Some(pos) = links.iter().position(|&(x, y)| x == a && y == b) {
        links.remove(pos);
        false
    } else {
        links.push((a, b));
        true
    }
}

/// Toggle a "one destination per source" link: if the exact `(src, dst)` link
/// exists, remove it; otherwise drop any other link with the same `src` and add
/// this one. Returns whether the link is present afterward. Used for serve links
/// (one server per http node) and net links (one network per app).
pub fn toggle_unique(links: &mut Vec<(NodeId, NodeId)>, src: NodeId, dst: NodeId) -> bool {
    if let Some(pos) = links.iter().position(|&(s, d)| s == src && d == dst) {
        links.remove(pos);
        false
    } else {
        links.retain(|&(s, _)| s != src);
        links.push((src, dst));
        true
    }
}

/// What must change for the set of running HTTP servers to match the desired
/// serve wiring. Produced by [`reconcile_serves`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ServePlan {
    /// http node ids whose running server must be stopped (its wiring changed or
    /// went away).
    pub stop: Vec<NodeId>,
    /// `(http, hostport)` links that aren't running yet and should be started.
    /// The caller may still skip some (node not ready, port already taken); those
    /// are retried on the next reconcile.
    pub start: Vec<(NodeId, NodeId)>,
}

/// Diff the desired `serve_links` against the currently-running servers
/// (`active`: http id → the HostPort it is bound through) and return what to stop
/// and start. Pure: the caller performs the actual bind/kill and applies its own
/// readiness/port-conflict guards to `start`.
///
/// A server bound through the *wrong* HostPort (its wiring changed) appears in
/// both `stop` and `start` — the caller kills it, then re-binds it on the new
/// port. Apply `stop` before `start`.
pub fn reconcile_serves(
    serve_links: &[(NodeId, NodeId)],
    active: &HashMap<NodeId, NodeId>,
) -> ServePlan {
    let desired = |http: NodeId| {
        serve_links
            .iter()
            .find(|&&(h, _)| h == http)
            .map(|&(_, hp)| hp)
    };
    let stop: Vec<NodeId> = active
        .iter()
        .filter(|(&http, &hp)| desired(http) != Some(hp))
        .map(|(&http, _)| http)
        .collect();
    let stopped: HashSet<NodeId> = stop.iter().copied().collect();
    // Start every desired link whose http won't still be running after the stops
    // — either it was never active, or it was just stopped (a re-bind).
    let start = serve_links
        .iter()
        .filter(|&&(h, _)| !active.contains_key(&h) || stopped.contains(&h))
        .copied()
        .collect();
    ServePlan { stop, start }
}

/// A plain set-diff plan for link-driven effects (file mounts, MIDI routes):
/// which desired links aren't applied yet, and which applied links are no longer
/// desired. Simpler than [`ServePlan`] — these effects have no readiness or
/// one-per constraint, so it's a pure set difference.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LinkPlan {
    /// Desired but not active — apply the effect (mount / route).
    pub add: Vec<(NodeId, NodeId)>,
    /// Active but no longer desired — tear it down (unmount / unroute).
    pub remove: Vec<(NodeId, NodeId)>,
}

/// Diff a desired link list against the set currently applied. The caller
/// performs the effects; a desired link whose node isn't resolvable yet simply
/// stays in `add` and is retried on the next reconcile.
pub fn reconcile_links(
    desired: &[(NodeId, NodeId)],
    active: &HashSet<(NodeId, NodeId)>,
) -> LinkPlan {
    let want: HashSet<(NodeId, NodeId)> = desired.iter().copied().collect();
    let add = want
        .iter()
        .copied()
        .filter(|p| !active.contains(p))
        .collect();
    let remove = active
        .iter()
        .copied()
        .filter(|p| !want.contains(p))
        .collect();
    LinkPlan { add, remove }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn id(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    #[test]
    fn classify_covers_every_class_pair() {
        use NodeClass::*;
        let (a, b) = (id(1), id(2));
        // A special node wires only to an app (Other) — except an uplink node,
        // which wires only to a Network; every other special↔special pairing
        // is None.
        let cases = [
            (File, File, None),
            (File, Port, None),
            (File, Net, None),
            (File, Uplink, None),
            (File, Other, Some(Wire::Bind(a, b))),
            (Port, File, None),
            (Port, Port, None),
            (Port, Net, None),
            (Port, Uplink, None),
            (Port, Other, Some(Wire::Serve(b, a))),
            (Net, File, None),
            (Net, Port, None),
            (Net, Net, None),
            (Net, Uplink, Some(Wire::Net(b, a))),
            (Net, Other, Some(Wire::Net(b, a))),
            (Uplink, File, None),
            (Uplink, Port, None),
            (Uplink, Net, Some(Wire::Net(a, b))),
            (Uplink, Uplink, None),
            (Uplink, Other, None),
            (Other, File, Some(Wire::Bind(b, a))),
            (Other, Port, Some(Wire::Serve(a, b))),
            (Other, Net, Some(Wire::Net(a, b))),
            (Other, Uplink, None),
            (Other, Other, Some(Wire::Midi(a, b))),
            // A hardware MIDI source drives an app; it's always the link's source
            // and never wires to another special node.
            (MidiSource, Other, Some(Wire::Midi(a, b))),
            (Other, MidiSource, Some(Wire::Midi(b, a))),
            (MidiSource, MidiSource, None),
            (MidiSource, Net, None),
            // A hardware MIDI sink is the mirror: always the destination.
            (MidiSink, Other, Some(Wire::Midi(b, a))),
            (Other, MidiSink, Some(Wire::Midi(a, b))),
            (MidiSink, MidiSink, None),
            (MidiSink, Net, None),
            // Hardware in straight to hardware out: a MIDI thru box.
            (MidiSource, MidiSink, Some(Wire::Midi(a, b))),
            (MidiSink, MidiSource, Some(Wire::Midi(b, a))),
            // An API node grants an app API access; the app is always the
            // wire's first element, and it never wires to another special node.
            (Api, Other, Some(Wire::Api(b, a))),
            (Other, Api, Some(Wire::Api(a, b))),
            (Api, Api, None),
            (Api, Net, None),
            // A Clipboard node grants an app the host clipboard; same shape
            // as Capture and Api — app first, never special-to-special.
            (Clipboard, Other, Some(Wire::Clipboard(b, a))),
            (Other, Clipboard, Some(Wire::Clipboard(a, b))),
            (Clipboard, Clipboard, None),
            (Clipboard, Capture, None),
            (Clipboard, Net, None),
            // A `group` node wires to nothing at all: an instance's edges are
            // the definition's ports, wired by name in the file's `group`
            // block, and the expansion collapses them into wires between real
            // nodes. Dragging onto the instance node itself connects nothing.
            (Instance, Other, None),
            (Other, Instance, None),
            (Instance, File, None),
            (Instance, Instance, None),
        ];
        for (ca, cb, want) in cases {
            assert_eq!(classify(a, b, ca, cb), want, "classify({ca:?}, {cb:?})");
        }
    }

    /// Every class a node's kind can be *inferred* to have — the ones the
    /// pre-boundary-port rules were written for.
    fn any_plain_class() -> impl Strategy<Value = NodeClass> {
        prop_oneof![
            Just(NodeClass::File),
            Just(NodeClass::Port),
            Just(NodeClass::Net),
            Just(NodeClass::Uplink),
            Just(NodeClass::Capture),
            Just(NodeClass::Clipboard),
            Just(NodeClass::Api),
            Just(NodeClass::MidiSource),
            Just(NodeClass::MidiSink),
            Just(NodeClass::Instance),
            Just(NodeClass::Other),
        ]
    }

    fn any_port_kind() -> impl Strategy<Value = PortKind> {
        prop::sample::select(PortKind::ALL.to_vec())
    }

    fn any_dir() -> impl Strategy<Value = PortDir> {
        prop_oneof![Just(PortDir::In), Just(PortDir::Out)]
    }

    /// Any class at all, boundary ports included.
    fn any_class() -> impl Strategy<Value = NodeClass> {
        prop_oneof![
            9 => any_plain_class(),
            4 => (any_dir(), any_port_kind()).prop_map(|(d, k)| NodeClass::Boundary(d, k)),
        ]
    }

    /// What a wire carries — the port kind it would cross a boundary as.
    fn wire_kind(w: Wire) -> PortKind {
        match w {
            Wire::Bind(..) => PortKind::Bind,
            Wire::Midi(..) => PortKind::Midi,
            Wire::Serve(..) => PortKind::Serve,
            Wire::Net(..) => PortKind::Net,
            Wire::Capture(..) => PortKind::Capture,
            Wire::Clipboard(..) => PortKind::Clipboard,
            Wire::Api(..) => PortKind::Api,
        }
    }

    fn any_id() -> impl Strategy<Value = NodeId> {
        any::<u128>().prop_map(NodeId::from_u128)
    }

    #[test]
    fn a_boundary_port_stands_in_for_the_node_on_the_far_side() {
        use NodeClass::*;
        use PortDir::{In, Out};
        let (a, b) = (id(1), id(2));
        let boundary = |dir, kind| Boundary(dir, kind);
        let cases = [
            // An in-port supplies what an app consumes, so it takes the
            // supplier's end: expansion replaces it with the node the parent
            // wired in, and `X -> port -> app` collapses to `X -> app`.
            (
                boundary(In, PortKind::Bind),
                Other,
                Some(Wire::Bind(a, b)),
                "a volume arriving from outside",
            ),
            (
                boundary(In, PortKind::Midi),
                Other,
                Some(Wire::Midi(a, b)),
                "notes arriving from outside",
            ),
            (
                boundary(In, PortKind::Capture),
                Other,
                Some(Wire::Capture(b, a)),
                "a capture grant arriving from outside",
            ),
            (
                boundary(In, PortKind::Clipboard),
                Other,
                Some(Wire::Clipboard(b, a)),
                "a clipboard grant from outside",
            ),
            (
                boundary(In, PortKind::Api),
                Other,
                Some(Wire::Api(b, a)),
                "an API grant from outside",
            ),
            // An out-port stands in for the consumer on the far side, so the
            // inner node keeps the producer's end of the wire.
            (
                File,
                boundary(Out, PortKind::Bind),
                Some(Wire::Bind(a, b)),
                "an inner volume exposed outward",
            ),
            (
                Other,
                boundary(Out, PortKind::Bind),
                Some(Wire::Bind(a, b)),
                "an inner fs provider exposed outward",
            ),
            (
                Other,
                boundary(Out, PortKind::Midi),
                Some(Wire::Midi(a, b)),
                "an inner app's notes leaving",
            ),
            (
                MidiSource,
                boundary(Out, PortKind::Midi),
                Some(Wire::Midi(a, b)),
                "inner hardware MIDI leaving",
            ),
            (
                MidiSink,
                boundary(In, PortKind::Midi),
                Some(Wire::Midi(b, a)),
                "notes arriving from outside, played out of a hardware port",
            ),
            (
                Capture,
                boundary(Out, PortKind::Capture),
                Some(Wire::Capture(b, a)),
                "an inner Capture node offered outward",
            ),
            (
                Api,
                boundary(Out, PortKind::Api),
                Some(Wire::Api(b, a)),
                "an inner Api node offered outward",
            ),
            // A port wired to the wrong class is refused, not reclassified —
            // a `midi` port meeting a volume is a mistake, not a bind.
            (
                boundary(In, PortKind::Midi),
                File,
                None,
                "a midi port is not a mount point",
            ),
            (
                boundary(In, PortKind::Bind),
                Net,
                None,
                "a bind port does not join a network",
            ),
            (
                boundary(Out, PortKind::Capture),
                Other,
                None,
                "an app is not a capture source",
            ),
            (
                boundary(Out, PortKind::Bind),
                Net,
                None,
                "a network is not a filesystem",
            ),
            // Two ports of the same kind on opposite edges pass a connection
            // straight through the definition, with no inner node in between.
            (
                boundary(In, PortKind::Bind),
                boundary(Out, PortKind::Bind),
                Some(Wire::Bind(a, b)),
                "a bind passed through",
            ),
            (
                boundary(Out, PortKind::Midi),
                boundary(In, PortKind::Midi),
                Some(Wire::Midi(b, a)),
                "notes passed through, written the other way round",
            ),
            // ...but only of the same kind, and only opposite edges.
            (
                boundary(In, PortKind::Bind),
                boundary(Out, PortKind::Midi),
                None,
                "kinds must match",
            ),
            (
                boundary(In, PortKind::Midi),
                boundary(In, PortKind::Midi),
                None,
                "two inputs have nothing to say to each other",
            ),
            // Net and serve ports are refused at load; if one ever reached the
            // canvas it must still not wire.
            (
                boundary(In, PortKind::Net),
                Net,
                None,
                "net ports are not supported yet",
            ),
            (
                boundary(Out, PortKind::Serve),
                Port,
                None,
                "serve ports are not supported yet",
            ),
        ];
        for (ca, cb, want, why) in cases {
            assert_eq!(classify(a, b, ca, cb), want, "{why}: ({ca:?}, {cb:?})");
        }
    }

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

    proptest! {
        /// A classified wire always joins exactly the two input nodes (never
        /// invents or drops an endpoint), regardless of orientation.
        #[test]
        fn classify_preserves_endpoints(a in any_id(), b in any_id(), ca in any_class(), cb in any_class()) {
            if let Some(w) = classify(a, b, ca, cb) {
                let (x, y) = wire_ends(w);
                prop_assert!((x == a && y == b) || (x == b && y == a));
            }
        }

        /// A boundary port only ever forms a wire of the kind it declares. This
        /// is the whole point of a *typed* boundary: a port wired to a node of
        /// the wrong class is refused, never quietly reclassified into whatever
        /// that node happens to accept.
        #[test]
        fn a_boundary_port_only_forms_the_kind_it_declares(
            a in any_id(),
            b in any_id(),
            dir in any_dir(),
            kind in any_port_kind(),
            oc in any_class(),
        ) {
            let port = NodeClass::Boundary(dir, kind);
            // Either argument order: the canvas hands over whichever node the
            // user dragged from.
            for (ca, cb) in [(port, oc), (oc, port)] {
                if let Some(w) = classify(a, b, ca, cb) {
                    prop_assert_eq!(wire_kind(w), kind);
                }
            }
        }

        /// A wire forms if and only if one side is an app node paired with a
        /// non-Uplink node, or the pair is one of the two special-to-special
        /// pairs that mean something on their own: an Uplink and a Network, or
        /// a hardware MIDI input and a hardware MIDI output (a thru box — a
        /// keyboard playing an external sound module through wk). A `group`
        /// node is outside all of it: an instance is wired through the
        /// definition's ports, by name, in the file — never by a live wire to
        /// the instance node. (Boundary ports have their own rule above; this
        /// one is about the inferred classes.)
        #[test]
        fn classify_requires_an_app_or_uplink_endpoint(a in any_id(), b in any_id(), ca in any_plain_class(), cb in any_plain_class()) {
            use NodeClass::*;
            let wired = classify(a, b, ca, cb).is_some();
            let instance = ca == Instance || cb == Instance;
            let app_pair = (ca == Other || cb == Other) && ca != Uplink && cb != Uplink;
            let uplink_pair = matches!((ca, cb), (Uplink, Net) | (Net, Uplink));
            let midi_thru = matches!((ca, cb), (MidiSource, MidiSink) | (MidiSink, MidiSource));
            prop_assert_eq!(wired, !instance && (app_pair || uplink_pair || midi_thru));
        }

        /// Toggling the same pair twice restores the original link set, and a
        /// single toggle flips its presence.
        #[test]
        fn toggle_pair_is_an_involution(
            mut links in prop::collection::vec((any_id(), any_id()), 0..8),
            a in any_id(),
            b in any_id(),
        ) {
            let before = links.clone();
            let connected = toggle_pair(&mut links, a, b);
            prop_assert_eq!(connected, links.contains(&(a, b)));
            toggle_pair(&mut links, a, b);
            // Order can differ (remove+push), so compare as multisets by sorting.
            let mut got = links.clone();
            let mut want = before.clone();
            got.sort();
            want.sort();
            prop_assert_eq!(got, want);
        }

        /// After `toggle_unique` connects `(src, dst)`, `src` appears exactly once
        /// — the "one destination per source" invariant.
        #[test]
        fn toggle_unique_keeps_one_dest_per_source(
            mut links in prop::collection::vec((any_id(), any_id()), 0..8),
            src in any_id(),
            dst in any_id(),
        ) {
            let connected = toggle_unique(&mut links, src, dst);
            let with_src = links.iter().filter(|&&(s, _)| s == src).count();
            if connected {
                prop_assert_eq!(with_src, 1);
                prop_assert!(links.contains(&(src, dst)));
            } else {
                prop_assert_eq!(with_src, 0);
            }
        }
    }

    // Build an `active` map (one hostport per http) plus its `serve_links` view.
    fn serve_state() -> impl Strategy<Value = (Vec<(NodeId, NodeId)>, HashMap<NodeId, NodeId>)> {
        (
            prop::collection::hash_map(any_id(), any_id(), 0..6),
            prop::collection::hash_map(any_id(), any_id(), 0..6),
        )
            .prop_map(|(links_map, active)| {
                let links: Vec<(NodeId, NodeId)> = links_map.into_iter().collect();
                (links, active)
            })
    }

    proptest! {
        /// A plan only stops running servers and only starts desired links, and
        /// no started http remains running after the stops are applied (so the
        /// caller never double-binds). A wrong-port server may be both stopped and
        /// started — that is a legitimate re-bind.
        #[test]
        fn reconcile_plan_is_well_formed((links, active) in serve_state()) {
            let plan = reconcile_serves(&links, &active);
            let stopped: std::collections::HashSet<_> = plan.stop.iter().copied().collect();
            for http in &plan.stop {
                prop_assert!(active.contains_key(http), "stopped a server that wasn't running");
            }
            for pair in &plan.start {
                prop_assert!(links.contains(pair), "started a link that isn't desired");
                let (http, _) = *pair;
                // Won't still be running once the stops are applied.
                prop_assert!(!active.contains_key(&http) || stopped.contains(&http));
            }
        }

        /// Applying the plan (kill the stops, bind the starts) yields an `active`
        /// map that exactly matches the desired serve links — the reconcile
        /// converges in one pass when every start succeeds.
        #[test]
        fn applying_plan_reaches_desired_state((links, active) in serve_state()) {
            let plan = reconcile_serves(&links, &active);
            let mut result = active.clone();
            for http in &plan.stop {
                result.remove(http);
            }
            for &(http, hp) in &plan.start {
                result.insert(http, hp);
            }
            let desired: HashMap<NodeId, NodeId> = links.iter().copied().collect();
            prop_assert_eq!(result, desired);
        }

        /// Reconciling an already-consistent state proposes no changes.
        #[test]
        fn reconcile_is_idempotent_at_fixpoint(links in prop::collection::hash_map(any_id(), any_id(), 0..6)) {
            let serve_links: Vec<(NodeId, NodeId)> = links.iter().map(|(&h, &hp)| (h, hp)).collect();
            let plan = reconcile_serves(&serve_links, &links);
            prop_assert_eq!(plan, ServePlan::default());
        }

        /// A link plan only adds desired-but-inactive links and only removes
        /// active-but-undesired ones, with no overlap.
        #[test]
        fn link_plan_is_well_formed(
            desired in prop::collection::vec((any_id(), any_id()), 0..8),
            active in prop::collection::hash_set((any_id(), any_id()), 0..8),
        ) {
            let want: HashSet<(NodeId, NodeId)> = desired.iter().copied().collect();
            let plan = reconcile_links(&desired, &active);
            for p in &plan.add {
                prop_assert!(want.contains(p) && !active.contains(p));
            }
            for p in &plan.remove {
                prop_assert!(active.contains(p) && !want.contains(p));
            }
        }

        /// Applying the plan (add the adds, drop the removes) makes the active set
        /// exactly the desired set — reconcile converges in one pass.
        #[test]
        fn applying_link_plan_reaches_desired_state(
            desired in prop::collection::vec((any_id(), any_id()), 0..8),
            active in prop::collection::hash_set((any_id(), any_id()), 0..8),
        ) {
            let plan = reconcile_links(&desired, &active);
            let mut result = active.clone();
            for p in &plan.remove {
                result.remove(p);
            }
            for p in &plan.add {
                result.insert(*p);
            }
            let want: HashSet<(NodeId, NodeId)> = desired.iter().copied().collect();
            prop_assert_eq!(result, want);
        }

        /// Reconciling an already-consistent set proposes no changes.
        #[test]
        fn link_reconcile_is_idempotent_at_fixpoint(
            active in prop::collection::hash_set((any_id(), any_id()), 0..8),
        ) {
            let desired: Vec<(NodeId, NodeId)> = active.iter().copied().collect();
            prop_assert_eq!(reconcile_links(&desired, &active), LinkPlan::default());
        }
    }
}
