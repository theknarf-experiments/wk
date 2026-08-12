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
    // The attempted operation is an ambient fact — the same `operation(...)`
    // vocabulary the node-capability plane uses — and a single static policy
    // allows it iff the token grants the matching right.
    let authorizer = authorizer!(
        r#"
        operation({res}, {act});
        allow if operation($r, $a), right($r, $a);
        "#
    );
    match authorizer.build(&token) {
        Ok(mut a) => authorized(&mut a),
        Err(_) => false,
    }
}

/// Decide whether a *node* holding `token_bytes` may perform `action` on
/// `(kind, target)` — the node-capability half of authorization, gating what a
/// node's wires actually grant (a file mount and its writability, a HostPort
/// publish, a network join, host access via a gateway, a MIDI direction).
///
/// The policy lives in the **token**, not here: the server only supplies the
/// world as ambient facts — `wired(kind, target)` for every wire the node
/// currently has, plus the attempted `operation(kind, target, action)` — and
/// one static policy allowing the operation iff the token's Datalog derives
/// `can_use` for it. The default token's authority block carries
/// `can_use($k, $t, $a) <- wired($k, $t)` ("use what you're connected to, in
/// every mode"); attenuation blocks append checks that narrow it (a kind, a
/// target, or an action — e.g. read-only), and a replacement token can carry
/// different logic entirely.
pub fn authorize_use(
    public_key: PublicKey,
    token_bytes: &[u8],
    wired: &[(&str, String)],
    kind: &str,
    target: &str,
    action: &str,
) -> bool {
    let token = match Biscuit::from(token_bytes, public_key) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let mut builder = authorizer!(
        r#"
        operation({kind}, {target}, {action});
        allow if operation($k, $t, $a), can_use($k, $t, $a);
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
            .code(
                "can_use($kind, $target, $action) <- wired($kind, $target), \
                 operation($kind, $target, $action);\n\
                 can_use(\"scene\", $target, $action) <- operation(\"scene\", $target, $action);\n\
                 can_use(\"exec\", $target, $action) <- operation(\"exec\", $target, $action);",
            )
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
        let ok = |w: &[(&str, String)], k: &str, t: &str, a: &str| {
            authorize_use(root.public(), &token, w, k, t, a)
        };
        // Wired grants every action on the wire's target.
        assert!(ok(&wired, "file", "vol1", "read"));
        assert!(ok(&wired, "file", "vol1", "write"));
        assert!(ok(&wired, "net", "lan", "use"));
        // Not wired: no fact, so the rule derives nothing.
        assert!(!ok(&wired, "port", "hp1", "use"));
        assert!(!ok(&wired, "file", "other", "read"));
        // No wires at all: nothing is usable.
        assert!(!ok(&[], "file", "vol1", "read"));
    }

    /// Append one attenuation block to a token, holder-side (no key).
    fn attenuate(token: &[u8], block: &str) -> Vec<u8> {
        biscuit_auth::UnverifiedBiscuit::from(token)
            .unwrap()
            .append(
                biscuit_auth::builder::BlockBuilder::new()
                    .code(block)
                    .unwrap(),
            )
            .unwrap()
            .to_vec()
            .unwrap()
    }

    #[test]
    fn attenuation_narrows_without_the_private_key() {
        let root = KeyPair::new();
        let token = mint_node_base(&root);
        // A holder (no signing key) appends a check: never the network.
        let attenuated = attenuate(&token, r#"check if operation($k, $t, $a), $k != "net";"#);
        let wired = [("file", "vol1".to_string()), ("net", "lan".to_string())];
        // Still usable: the wired file.
        assert!(authorize_use(
            root.public(),
            &attenuated,
            &wired,
            "file",
            "vol1",
            "write"
        ));
        // Cut off: the network, even though it is wired.
        assert!(!authorize_use(
            root.public(),
            &attenuated,
            &wired,
            "net",
            "lan",
            "use"
        ));
    }

    #[test]
    fn read_only_attenuation_keeps_reads_and_drops_writes() {
        let root = KeyPair::new();
        let token = mint_node_base(&root);
        // Read-only: every operation must be a non-file one, or a file read.
        let ro = attenuate(
            &token,
            r#"check if operation($k, $t, $a), $k != "file" || $a == "read";"#,
        );
        let wired = [("file", "vol1".to_string()), ("net", "lan".to_string())];
        assert!(authorize_use(
            root.public(),
            &ro,
            &wired,
            "file",
            "vol1",
            "read"
        ));
        assert!(!authorize_use(
            root.public(),
            &ro,
            &wired,
            "file",
            "vol1",
            "write"
        ));
        // Other kinds are untouched.
        assert!(authorize_use(
            root.public(),
            &ro,
            &wired,
            "net",
            "lan",
            "use"
        ));
    }

    #[test]
    fn scene_show_is_allowed_without_a_wire_and_mutable_away() {
        let root = KeyPair::new();
        let token = mint_node_base(&root);
        // No wires at all: scene "show" still passes (the default is all-allow;
        // an entity needs no wire), while everything else stays wire-gated.
        assert!(authorize_use(
            root.public(),
            &token,
            &[],
            "scene",
            "node1",
            "show"
        ));
        assert!(!authorize_use(
            root.public(),
            &token,
            &[],
            "file",
            "node1",
            "read"
        ));
        // The mute: one check kills scene output, nothing else.
        let muted = attenuate(&token, r#"check if operation($k, $t, $a), $k != "scene";"#);
        let wired = [("file", "vol1".to_string())];
        assert!(!authorize_use(
            root.public(),
            &muted,
            &[],
            "scene",
            "node1",
            "show"
        ));
        assert!(authorize_use(
            root.public(),
            &muted,
            &wired,
            "file",
            "vol1",
            "read"
        ));
    }

    #[test]
    fn gateway_is_a_distinct_kind_from_net() {
        let root = KeyPair::new();
        let token = mint_node_base(&root);
        // Wired to a plain net and to a gateway; cut off host access only.
        let no_gateway = attenuate(
            &token,
            r#"check if operation($k, $t, $a), $k != "gateway";"#,
        );
        let wired = [("net", "lan".to_string()), ("gateway", "gw".to_string())];
        assert!(authorize_use(
            root.public(),
            &no_gateway,
            &wired,
            "net",
            "lan",
            "use"
        ));
        assert!(!authorize_use(
            root.public(),
            &no_gateway,
            &wired,
            "gateway",
            "gw",
            "use"
        ));
    }

    #[test]
    fn a_swapped_authority_replaces_the_logic() {
        let root = KeyPair::new();
        // A token with no wiring rule at all — a deny-everything policy —
        // and one granting a single fixed (target, action) regardless of wiring.
        let deny_all = Biscuit::builder().build(&root).unwrap().to_vec().unwrap();
        let fixed = Biscuit::builder()
            .code(r#"can_use("file", "vol1", "read");"#)
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
            "vol1",
            "read"
        ));
        assert!(authorize_use(
            root.public(),
            &fixed,
            &wired,
            "file",
            "vol1",
            "read"
        ));
        assert!(!authorize_use(
            root.public(),
            &fixed,
            &wired,
            "file",
            "vol1",
            "write"
        ));
        assert!(!authorize_use(
            root.public(),
            &fixed,
            &wired,
            "net",
            "lan",
            "use"
        ));
        // The fixed grant holds even with no wire present — the logic swapped.
        assert!(authorize_use(
            root.public(),
            &fixed,
            &[],
            "file",
            "vol1",
            "read"
        ));
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
            "vol1",
            "read"
        ));
        assert!(!authorize_use(
            root.public(),
            b"garbage",
            &wired,
            "file",
            "vol1",
            "read"
        ));
    }
}
