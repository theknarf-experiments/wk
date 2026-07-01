//! Command authorization — the *verifying* half of the auth split. The server
//! never mints tokens or holds a signing key; it is handed only a
//! [`PublicKey`](biscuit_auth::PublicKey) (a copy of the token service's public
//! key) and uses it to verify + authorize every command against the
//! [Biscuit](https://www.biscuitsec.org/) token that carried it.
//!
//! Authorization is resource + action shaped: a token grants
//! `right(resource, action)` facts and each command names the pair it needs
//! ([`wk_protocol::Command::required`]). Minting and key management live in the
//! separate `wk-token-service` crate; a client bears a token it was issued and
//! presents it with each action.

use biscuit_auth::macros::authorizer;
use biscuit_auth::{Biscuit, PublicKey};

use wk_protocol::{Action, ResourceKind};

/// Verify `token_bytes` against `public_key` and decide whether the holder may
/// perform `action` on `resource`. Returns `false` on a bad signature, a
/// malformed token, or an insufficient grant — the caller then drops the
/// command.
pub fn authorize(
    public_key: PublicKey,
    token_bytes: &[u8],
    resource: ResourceKind,
    action: Action,
) -> bool {
    // Deserializing with the public key verifies the token's signature chain.
    let token = match Biscuit::from(token_bytes, public_key) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let res = resource.as_str();
    let act = action.as_str();
    // The attempted (resource, action) are facts; a single static policy allows
    // them iff the token grants the matching right.
    let authorizer = authorizer!(
        r#"
        resource({res});
        action({act});
        allow if resource($r), action($a), right($r, $a);
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
    fn mint(root: &KeyPair, rights: &[(ResourceKind, Action)]) -> Vec<u8> {
        let mut b = Biscuit::builder();
        for (res, act) in rights {
            b = b
                .fact(format!(r#"right("{}", "{}")"#, res.as_str(), act.as_str()).as_str())
                .unwrap();
        }
        b.build(root).unwrap().to_vec().unwrap()
    }

    fn mint_admin(root: &KeyPair) -> Vec<u8> {
        let all: Vec<(ResourceKind, Action)> = ResourceKind::ALL
            .iter()
            .flat_map(|&r| Action::ALL.iter().map(move |&a| (r, a)))
            .collect();
        mint(root, &all)
    }

    #[test]
    fn admin_token_allows_every_pair() {
        let root = KeyPair::new();
        let token = mint_admin(&root);
        for res in ResourceKind::ALL {
            for act in Action::ALL {
                assert!(
                    authorize(root.public(), &token, res, act),
                    "denied {res:?} {act:?}"
                );
            }
        }
    }

    #[test]
    fn scoped_token_allows_only_its_pairs() {
        let root = KeyPair::new();
        // A wire-only client: may connect and disconnect, nothing else.
        let token = mint(
            &root,
            &[
                (ResourceKind::Wire, Action::Create),
                (ResourceKind::Wire, Action::Delete),
            ],
        );
        assert!(authorize(
            root.public(),
            &token,
            ResourceKind::Wire,
            Action::Create
        ));
        assert!(authorize(
            root.public(),
            &token,
            ResourceKind::Wire,
            Action::Delete
        ));
        // Same action, different resource: denied.
        assert!(!authorize(
            root.public(),
            &token,
            ResourceKind::Node,
            Action::Create
        ));
        // Same resource, different action: denied (no such right minted).
        assert!(!authorize(
            root.public(),
            &token,
            ResourceKind::Wire,
            Action::Update
        ));
    }

    #[test]
    fn arrange_does_not_grant_update() {
        let root = KeyPair::new();
        // A layout-only client: may move/resize nodes but not reconfigure them.
        let token = mint(&root, &[(ResourceKind::Node, Action::Arrange)]);
        assert!(authorize(
            root.public(),
            &token,
            ResourceKind::Node,
            Action::Arrange
        ));
        assert!(!authorize(
            root.public(),
            &token,
            ResourceKind::Node,
            Action::Update
        ));
    }

    #[test]
    fn token_from_a_different_root_is_rejected() {
        let root = KeyPair::new();
        let attacker = KeyPair::new();
        // A valid, full-authority token — but signed by the wrong root.
        let forged = mint_admin(&attacker);
        assert!(!authorize(
            root.public(),
            &forged,
            ResourceKind::Node,
            Action::Arrange
        ));
    }

    #[test]
    fn garbage_token_is_rejected() {
        let root = KeyPair::new();
        assert!(!authorize(
            root.public(),
            b"not a token",
            ResourceKind::Node,
            Action::Arrange
        ));
    }
}
