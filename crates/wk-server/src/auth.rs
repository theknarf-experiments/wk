//! Command authorization — the *verifying* half of the auth split. The server
//! never mints tokens or holds a signing key; it is handed only a
//! [`PublicKey`](biscuit_auth::PublicKey) (a copy of the token service's public
//! key) and uses it to verify + authorize every command against the
//! [Biscuit](https://www.biscuitsec.org/) token that carried it.
//!
//! Minting and key management live in the separate `wk-token-service` crate; a
//! client bears a token it was issued and presents it with each action.

use biscuit_auth::macros::authorizer;
use biscuit_auth::{Biscuit, PublicKey};

use wk_protocol::Operation;

/// Verify `token_bytes` against `public_key` and decide whether the holder may
/// perform `op`. Returns `false` on a bad signature, a malformed token, or an
/// insufficient grant — the caller then drops the command.
pub fn authorize(public_key: PublicKey, token_bytes: &[u8], op: Operation) -> bool {
    // Deserializing with the public key verifies the token's signature chain.
    let token = match Biscuit::from(token_bytes, public_key) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let op = op.as_str();
    // The attempted operation is a fact; a single static policy allows it iff the
    // token grants the matching right.
    let authorizer = authorizer!(
        r#"
        operation({op});
        allow if operation($o), right($o);
        "#
    );
    match authorizer.build(&token) {
        Ok(mut a) => a.authorize().is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use biscuit_auth::KeyPair;

    /// Mint a token granting `rights`, the way the token service would.
    fn mint(root: &KeyPair, rights: &[Operation]) -> Vec<u8> {
        let mut b = Biscuit::builder();
        for r in rights {
            b = b
                .fact(format!(r#"right("{}")"#, r.as_str()).as_str())
                .unwrap();
        }
        b.build(root).unwrap().to_vec().unwrap()
    }

    #[test]
    fn admin_token_allows_every_operation() {
        let root = KeyPair::new();
        let token = mint(&root, &Operation::ALL);
        for op in Operation::ALL {
            assert!(authorize(root.public(), &token, op), "denied {op:?}");
        }
    }

    #[test]
    fn scoped_token_allows_only_its_rights() {
        let root = KeyPair::new();
        let token = mint(&root, &[Operation::Arrange]);
        assert!(authorize(root.public(), &token, Operation::Arrange));
        assert!(!authorize(root.public(), &token, Operation::Remove));
        assert!(!authorize(root.public(), &token, Operation::Create));
    }

    #[test]
    fn token_from_a_different_root_is_rejected() {
        let root = KeyPair::new();
        let attacker = KeyPair::new();
        // A valid, full-authority token — but signed by the wrong root.
        let forged = mint(&attacker, &Operation::ALL);
        assert!(!authorize(root.public(), &forged, Operation::Arrange));
    }

    #[test]
    fn garbage_token_is_rejected() {
        let root = KeyPair::new();
        assert!(!authorize(
            root.public(),
            b"not a token",
            Operation::Arrange
        ));
    }
}
