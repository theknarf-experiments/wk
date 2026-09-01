//! Names for nodes: what a node is *called*, as opposed to what it *is*.
//!
//! A node's type is the dependency it runs — `python`, `synth` — and several
//! nodes can share one, so a type cannot be an identity. wk borrows docker's
//! answer: a node gets a two-word name of its own, unrelated to its type, and
//! you rename it when you care what it is called.
//!
//! Unlike docker's, the name is *derived from the node's id* rather than drawn
//! at random. The id is already in the `.wk` file, so the name never has to be
//! written down, never drifts, and is the same in every client looking at the
//! same workspace — a generated name is a fact about a node, not a decision
//! someone made about it.

use wk_protocol::NodeId;

/// Qualities, deliberately bland: a name is a handle, and a handle that reads
/// as an opinion about the node ages badly.
const QUALITIES: [&str; 32] = [
    "amber", "bright", "brisk", "calm", "clever", "cosy", "curious", "deep", "eager", "easy",
    "fair", "gentle", "glad", "keen", "kind", "lively", "lucky", "merry", "mild", "neat", "nimble",
    "patient", "plain", "quick", "quiet", "ready", "smooth", "soft", "steady", "sunny", "warm",
    "wise",
];

/// Things, not people: a node named after someone real invites a claim about
/// them that a word list has no business making.
const THINGS: [&str; 64] = [
    "anchor", "arbor", "basin", "beacon", "birch", "bluff", "brook", "canyon", "cedar", "cinder",
    "cliff", "comet", "cove", "creek", "delta", "dune", "ember", "fern", "fjord", "forge",
    "garden", "glade", "grove", "harbor", "haven", "hollow", "island", "jetty", "lagoon",
    "lantern", "ledge", "maple", "meadow", "meander", "mesa", "moor", "narrows", "orbit",
    "orchard", "pebble", "pier", "pine", "plateau", "prairie", "quarry", "rapids", "reef", "ridge",
    "river", "shoal", "signal", "spire", "spring", "summit", "terrace", "thicket", "tide",
    "trellis", "tundra", "valley", "vault", "wharf", "willow", "window",
];

/// The name `id` generates: `quiet-harbor`, `nimble-tide`. Hyphenated rather
/// than docker's underscore because a node name is also its address on the
/// fabric, and a hostname may not contain an underscore.
///
/// `nth` walks to another name for the same id, for the case where two ids
/// land on the same pair — 4096 pairs is plenty for a workspace and far too
/// few to assume uniqueness.
pub fn generated(id: NodeId, nth: usize) -> String {
    // Hashed, not sliced. Node ids are UUIDv7: two created moments apart share
    // their leading bits, so taking the words from the id's own halves gave
    // every node in a workspace the same first word — and with one word doing
    // all the varying, collisions stopped being rare.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"wk:node-name:v1");
    h.update(id.as_u128().to_be_bytes());
    h.update((nth as u64).to_be_bytes());
    let d = h.finalize();
    let quality = QUALITIES[d[0] as usize % QUALITIES.len()];
    let thing = THINGS[d[1] as usize % THINGS.len()];
    format!("{quality}-{thing}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the whole module: a name says nothing about the type, so
    /// two nodes of one type are two obviously different things.
    #[test]
    fn a_name_is_stable_for_an_id_and_says_nothing_about_the_type() {
        let a = NodeId::from_u128(1);
        let b = NodeId::from_u128(2);
        assert_eq!(generated(a, 0), generated(a, 0), "same id, same name");
        assert_ne!(generated(a, 0), generated(b, 0));
        assert!(generated(a, 0).contains('-'));
    }

    /// Walking finds a different name for the same id, which is what a
    /// collision does.
    #[test]
    fn walking_gives_another_name_for_the_same_id() {
        let id = NodeId::from_u128(7);
        let first = generated(id, 0);
        let second = generated(id, 1);
        assert_ne!(first, second);
        assert_eq!(generated(id, 1), second, "and it is still deterministic");
    }

    /// Ids made moments apart must not cluster. Node ids are UUIDv7 and share
    /// their leading bits, so a namer that took the words from the id's own
    /// halves gave every node in a workspace the same first word — and with
    /// one word doing all the varying, names collided constantly and every
    /// collision wrote a `name` line into the user's file.
    #[test]
    fn ids_made_moments_apart_do_not_cluster() {
        // The shape of a real run: consecutive ids, as `NodeId::new()` mints
        // them within one workspace.
        let base = NodeId::new().as_u128();
        let names: Vec<String> = (0..24)
            .map(|i| generated(NodeId::from_u128(base + i), 0))
            .collect();
        let firsts: std::collections::HashSet<&str> =
            names.iter().map(|n| n.split('-').next().unwrap()).collect();
        // A tenth of the space, not half: the bug this guards against produced
        // ONE distinct first word for a whole workspace, and a threshold set
        // near the statistical mean would flake in a suite that gates commits.
        assert!(
            firsts.len() >= 10,
            "24 consecutive ids should not share a handful of first words: {names:?}"
        );
        // Both words vary, so the whole 64×64 space is in play. Not that
        // collisions never happen — 24 names over 4096 pairs collide about one
        // time in eight, which is what the walk is for — but that they are the
        // birthday paradox rather than a namer that only ever varies one word.
        let seconds: std::collections::HashSet<&str> =
            names.iter().map(|n| n.split('-').nth(1).unwrap()).collect();
        assert!(seconds.len() >= 10, "second words cluster too: {names:?}");
    }

    /// A name has to be a legal hostname: it is what a peer dials over the
    /// fabric, not only what `wk ps` prints.
    #[test]
    fn every_word_is_hostname_safe() {
        for w in QUALITIES.iter().chain(THINGS.iter()) {
            assert!(
                w.chars().all(|c| c.is_ascii_lowercase()),
                "{w:?} is not a bare lowercase word"
            );
        }
    }
}
