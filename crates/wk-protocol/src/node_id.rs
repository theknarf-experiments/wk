//! Globally-unique node identifiers.

use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

/// The Crockford base32 alphabet (no I, L, O, U), used for the textual form.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A node identifier: a [UUIDv7](https://www.rfc-editor.org/rfc/rfc9562) (a
/// 48-bit millisecond timestamp plus randomness), rendered as 26-character
/// Crockford base32 (the ULID text form) in the workspace file.
///
/// Every peer mints ids independently with no coordination, so there is no
/// collision risk once workspaces are shared over a network. Because v7 is
/// time-ordered, ids sort by creation — and the base32 text sorts the same way.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(Uuid);

impl NodeId {
    /// Mint a fresh, time-ordered id.
    pub fn new() -> Self {
        NodeId(Uuid::now_v7())
    }

    /// The nil id (all zeros) — a sentinel for "no canvas node" (e.g. the store
    /// backing a throwaway HTTP request), never minted for a real node.
    pub const fn nil() -> Self {
        NodeId(Uuid::nil())
    }

    /// The raw 128-bit value — for deriving a stable pseudo-value from the id
    /// (e.g. a fabric IP octet), not for identity comparisons (use `==`).
    pub fn as_u128(self) -> u128 {
        self.0.as_u128()
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NodeId {
    /// 26-character Crockford base32, most-significant digit first (so the text
    /// sorts identically to the underlying bytes).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut n = self.0.as_u128();
        let mut buf = [0u8; 26];
        for slot in buf.iter_mut().rev() {
            *slot = CROCKFORD[(n & 0x1f) as usize];
            n >>= 5;
        }
        // `buf` is entirely ASCII drawn from CROCKFORD.
        f.write_str(std::str::from_utf8(&buf).unwrap())
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({self})")
    }
}

impl FromStr for NodeId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.chars().count() != 26 {
            return Err(format!("node id must be 26 base32 chars, got {:?}", s));
        }
        let mut n: u128 = 0;
        for c in s.chars() {
            let v = crockford_value(c).ok_or_else(|| format!("invalid base32 char {c:?}"))?;
            n = n
                .checked_mul(32)
                .and_then(|n| n.checked_add(v as u128))
                .ok_or_else(|| "node id out of range".to_string())?;
        }
        Ok(NodeId(Uuid::from_u128(n)))
    }
}

/// Decode one Crockford base32 digit (case-insensitive; O→0, I/L→1).
fn crockford_value(c: char) -> Option<u8> {
    let up = match c.to_ascii_uppercase() as u8 {
        b'O' => b'0',
        b'I' | b'L' => b'1',
        other => other,
    };
    CROCKFORD.iter().position(|&d| d == up).map(|p| p as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_base32() {
        let id = NodeId::new();
        let text = id.to_string();
        assert_eq!(text.len(), 26);
        assert_eq!(text.parse::<NodeId>().unwrap(), id);
    }

    #[test]
    fn text_sorts_like_creation_order() {
        // v7 is time-ordered, so a later id renders to a lexicographically larger
        // string (the base32 preserves byte order).
        let a = NodeId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = NodeId::new();
        assert!(a < b);
        assert!(a.to_string() < b.to_string());
    }

    #[test]
    fn decode_is_lenient_but_rejects_garbage() {
        let id = NodeId::new();
        let text = id.to_string();
        // Crockford aliases: lowercase + o/i/l for 0/1/1 decode the same.
        assert_eq!(text.to_lowercase().parse::<NodeId>().unwrap(), id);
        assert!("too-short".parse::<NodeId>().is_err());
        assert!("UUUUUUUUUUUUUUUUUUUUUUUUUU".parse::<NodeId>().is_err());
    }
}
