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
use wk_protocol::{NodeId, Wire};

/// What a node is, for the purpose of classifying a wire between two nodes. A
/// node is exactly one of these (file/port/net node sets are disjoint; anything
/// else — an app node, or a not-yet-known id — is [`NodeClass::Other`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeClass {
    File,
    Port,
    Net,
    /// An uplink node (Iroh/Veilid) — wires only to a Network (the net it extends).
    Uplink,
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
    match (ca, cb) {
        (File, Other) => Some(Wire::File(a, b)),
        (Other, File) => Some(Wire::File(b, a)),
        // The http node is the app side; the HostPort is the second element.
        (Port, Other) => Some(Wire::Serve(b, a)),
        (Other, Port) => Some(Wire::Serve(a, b)),
        // The app (or uplink) is the first element; the network the second.
        (Net, Other) | (Net, Uplink) => Some(Wire::Net(b, a)),
        (Other, Net) | (Uplink, Net) => Some(Wire::Net(a, b)),
        (Other, Other) => Some(Wire::Midi(a, b)),
        _ => None,
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
            (File, Other, Some(Wire::File(a, b))),
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
            (Other, File, Some(Wire::File(b, a))),
            (Other, Port, Some(Wire::Serve(a, b))),
            (Other, Net, Some(Wire::Net(a, b))),
            (Other, Uplink, None),
            (Other, Other, Some(Wire::Midi(a, b))),
        ];
        for (ca, cb, want) in cases {
            assert_eq!(classify(a, b, ca, cb), want, "classify({ca:?}, {cb:?})");
        }
    }

    fn any_class() -> impl Strategy<Value = NodeClass> {
        prop_oneof![
            Just(NodeClass::File),
            Just(NodeClass::Port),
            Just(NodeClass::Net),
            Just(NodeClass::Uplink),
            Just(NodeClass::Other),
        ]
    }

    fn any_id() -> impl Strategy<Value = NodeId> {
        any::<u128>().prop_map(NodeId::from_u128)
    }

    fn wire_ends(w: Wire) -> (NodeId, NodeId) {
        match w {
            Wire::File(a, b) | Wire::Midi(a, b) | Wire::Serve(a, b) | Wire::Net(a, b) => (a, b),
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

        /// A wire forms if and only if one side is an app node paired with a
        /// non-Uplink node, or the pair is an Uplink uplink and a Network — two
        /// special nodes otherwise never wire.
        #[test]
        fn classify_requires_an_app_or_uplink_endpoint(a in any_id(), b in any_id(), ca in any_class(), cb in any_class()) {
            use NodeClass::*;
            let wired = classify(a, b, ca, cb).is_some();
            let app_pair = (ca == Other || cb == Other) && ca != Uplink && cb != Uplink;
            let uplink_pair = matches!((ca, cb), (Uplink, Net) | (Net, Uplink));
            prop_assert_eq!(wired, app_pair || uplink_pair);
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
