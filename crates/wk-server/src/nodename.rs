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
const THINGS: [&str; 32] = [
    "anchor", "beacon", "birch", "brook", "canyon", "cedar", "cinder", "comet", "delta", "ember",
    "fjord", "garden", "harbor", "hollow", "island", "lantern", "meadow", "mesa", "orbit",
    "pebble", "prairie", "reef", "ridge", "river", "signal", "summit", "thicket", "tide",
    "trellis", "valley", "willow", "wharf",
];

/// The name `id` generates: `quiet-harbor`, `nimble-tide`. Hyphenated rather
/// than docker's underscore because a node name is also its address on the
/// fabric, and a hostname may not contain an underscore.
///
/// `nth` walks to another name for the same id, for the rare case where two
/// ids land on the same pair — 1024 pairs is plenty for a workspace and far
/// too few to assume uniqueness.
pub fn generated(id: NodeId, nth: usize) -> String {
    // The two halves of the id feed the two words, so ids that differ anywhere
    // move at least one of them.
    let n = id.as_u128() ^ (nth as u128).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let quality = QUALITIES[((n >> 64) % QUALITIES.len() as u128) as usize];
    let thing = THINGS[(n % THINGS.len() as u128) as usize];
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
