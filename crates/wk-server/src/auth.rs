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
use biscuit_auth::{Authorizer, Biscuit, PublicKey};

use wk_protocol::{Action, ResourceKind};

/// Run an authorizer with a generous time budget. Biscuit's default run limit
/// is 1ms of *wall clock* — tight enough that a loaded machine can spuriously
/// time out a trivial policy, and a spurious denial here is a dropped command
/// (or a memoized capability denial). Our policies are tiny; 100ms is
/// unreachable except by a genuinely pathological token, which it still stops.
fn authorized(a: &mut Authorizer) -> bool {
    let limits = biscuit_auth::datalog::RunLimits {
        max_time: std::time::Duration::from_millis(100),
        ..Default::default()
    };
    a.authorize_with_limits(limits).is_ok()
}

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
        Ok(mut a) => authorized(&mut a),
        Err(_) => false,
    }
}

/// Decide whether a *node* holding `token_bytes` may use `(kind, target)` — the
/// node-capability half of authorization, gating what a node's wires actually
/// grant (a file mount, a HostPort publish, a network join, a MIDI route).
///
/// The policy lives in the **token**, not here: the server only supplies the
/// world as ambient facts — `wired(kind, target)` for every wire the node
/// currently has, plus the attempted `operation(kind, target)` — and one
/// static policy allowing the operation iff the token's Datalog derives
/// `can_use` for it. The default token's authority block carries
/// `can_use($k, $t) <- wired($k, $t)` ("use what you're connected to");
/// attenuation blocks append checks that narrow it, and a replacement token
/// can carry different logic entirely.
pub fn authorize_use(
    public_key: PublicKey,
    token_bytes: &[u8],
    wired: &[(&str, String)],
    kind: &str,
    target: &str,
) -> bool {
    let token = match Biscuit::from(token_bytes, public_key) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let mut builder = authorizer!(
        r#"
        operation({kind}, {target});
        allow if operation($k, $t), can_use($k, $t);
        "#
    );
    for (k, t) in wired {
        // `k` is a fixed kind string and `t` a NodeId's ULID text (alphanumeric),
        // so interpolating into datalog source is injection-safe.
        builder = match builder.fact(format!(r#"wired("{k}", "{t}")"#).as_str()) {
            Ok(b) => b,
            Err(_) => return false,
        };
    }
    match builder.build(&token) {
        Ok(mut a) => authorized(&mut a),
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

    /// Mint a node token carrying the default wiring rule, the way the token
    /// service's `mint_node_base` does.
    fn mint_node_base(root: &KeyPair) -> Vec<u8> {
        Biscuit::builder()
            .code("can_use($kind, $target) <- wired($kind, $target);")
            .unwrap()
            .build(root)
            .unwrap()
            .to_vec()
            .unwrap()
    }

    #[test]
    fn base_node_token_allows_exactly_what_is_wired() {
        let root = KeyPair::new();
        let token = mint_node_base(&root);
        let wired = [("file", "vol1".to_string()), ("net", "lan".to_string())];
        assert!(authorize_use(root.public(), &token, &wired, "file", "vol1"));
        assert!(authorize_use(root.public(), &token, &wired, "net", "lan"));
        // Not wired: no fact, so the rule derives nothing.
        assert!(!authorize_use(root.public(), &token, &wired, "port", "hp1"));
        assert!(!authorize_use(
            root.public(),
            &token,
            &wired,
            "file",
            "other"
        ));
        // No wires at all: nothing is usable.
        assert!(!authorize_use(root.public(), &token, &[], "file", "vol1"));
    }

    #[test]
    fn attenuation_narrows_without_the_private_key() {
        let root = KeyPair::new();
        let token = mint_node_base(&root);
        // A holder (no signing key) appends a check: never the network.
        let attenuated = biscuit_auth::UnverifiedBiscuit::from(&token)
            .unwrap()
            .append(
                biscuit_auth::builder::BlockBuilder::new()
                    .code(r#"check if operation($k, $t), $k != "net";"#)
                    .unwrap(),
            )
            .unwrap()
            .to_vec()
            .unwrap();
        let wired = [("file", "vol1".to_string()), ("net", "lan".to_string())];
        // Still usable: the wired file.
        assert!(authorize_use(
            root.public(),
            &attenuated,
            &wired,
            "file",
            "vol1"
        ));
        // Cut off: the network, even though it is wired.
        assert!(!authorize_use(
            root.public(),
            &attenuated,
            &wired,
            "net",
            "lan"
        ));
    }

    #[test]
    fn a_swapped_authority_replaces_the_logic() {
        let root = KeyPair::new();
        // A token with no wiring rule at all — a deny-everything policy —
        // and one granting a single fixed target regardless of wiring.
        let deny_all = Biscuit::builder().build(&root).unwrap().to_vec().unwrap();
        let fixed = Biscuit::builder()
            .code(r#"can_use("file", "vol1");"#)
            .unwrap()
            .build(&root)
            .unwrap()
            .to_vec()
            .unwrap();
        let wired = [("file", "vol1".to_string()), ("net", "lan".to_string())];
        assert!(!authorize_use(
            root.public(),
            &deny_all,
            &wired,
            "file",
            "vol1"
        ));
        assert!(authorize_use(root.public(), &fixed, &wired, "file", "vol1"));
        assert!(!authorize_use(root.public(), &fixed, &wired, "net", "lan"));
        // The fixed grant holds even with no wire present — the logic swapped.
        assert!(authorize_use(root.public(), &fixed, &[], "file", "vol1"));
    }

    #[test]
    fn node_token_from_a_different_root_is_rejected() {
        let root = KeyPair::new();
        let attacker = KeyPair::new();
        let forged = mint_node_base(&attacker);
        let wired = [("file", "vol1".to_string())];
        assert!(!authorize_use(
            root.public(),
            &forged,
            &wired,
            "file",
            "vol1"
        ));
        assert!(!authorize_use(
            root.public(),
            b"garbage",
            &wired,
            "file",
            "vol1"
        ));
    }
}
