//! The wk **token service**: the minting authority in the three-way auth split.
//! It owns the root signing keypair, hands out its [`PublicKey`] (which the
//! server uses to verify), and mints [Biscuit](https://www.biscuitsec.org/)
//! tokens granting `right(resource, action)` pairs. It never verifies commands
//! and never runs the workspace — that is the server's job — and it is the only
//! component that holds a private key.
//!
//! Locally the CLI creates one service, gives the server a copy of its public
//! key, mints a token, and hands that token to the client. When wk grows real
//! networking this becomes a standalone service issuing (and attenuating) tokens
//! for remote clients.

use biscuit_auth::{Biscuit, KeyPair, PrivateKey};

pub use biscuit_auth::PublicKey;
pub use wk_protocol::{Action, ResourceKind};

/// The Datalog policy every node token starts from, two rules:
///
/// 1. a node may use exactly what it is wired to on the canvas, in every mode.
///    The server supplies `wired(kind, target)` facts from the graph and
///    checks one `operation(kind, target, action)` per grant — kinds are
///    `file`, `midi`, `port`, `net`, `gateway` (host access — its own kind, so
///    it can be cut off by matching, since Datalog has no negation),
///    `capture`; actions are `read`/`write` (file), `send`/`receive` (midi),
///    `read` (capture), and `use` (the rest).
/// 2. a node may run programs from its own filesystem (`wk:exec`) and always
///    show its `wk:scene` content — `scene` needs no wire
///    (target = the owning node's id, action = `show`), so the default is
///    all-allow. It's a rule rather than baked-in behavior so future policy
///    (shared vs. local-only entities, per-viewer muting) is an attenuation or
///    a re-mint away: `check if operation($k, $t, $a), $k != "scene"` mutes a
///    node's objects without touching the node itself. `exec` is likewise
///    all-allow and attenuable: a child inherits the parent's filesystem and
///    nothing more, so running one is not an escalation — but
///    `$k != "exec"` takes the ability away.
///
/// The rules live in the *token* so attenuation can narrow them — e.g.
/// read-only files: `check if operation($k, $t, $a), $k != "file" || $a ==
/// "read"` — and a swapped token can replace the logic wholesale.
pub const NODE_BASE_RULE: &str = r#"can_use($kind, $target, $action) <- wired($kind, $target), operation($kind, $target, $action);
can_use("scene", $target, $action) <- operation("scene", $target, $action);
can_use("exec", $target, $action) <- operation("exec", $target, $action);"#;

/// The token-issuing authority. Holds the root keypair; mints tokens.
pub struct TokenService {
    root: KeyPair,
}

impl TokenService {
    /// Generate a fresh root keypair. In a persistent deployment the key would be
    /// loaded from secure storage instead.
    pub fn new() -> Self {
        TokenService {
            root: KeyPair::new(),
        }
    }

    /// Load the root keypair persisted at `key_path`, or generate one and save
    /// it there. Persisting the key makes tokens durable: an attenuated node
    /// token stored in the workspace still verifies after a restart. A
    /// malformed key file is replaced (with a warning) rather than fatal.
    pub fn load_or_create(key_path: &std::path::Path) -> Self {
        if let Ok(text) = std::fs::read_to_string(key_path) {
            match PrivateKey::from_bytes_hex(text.trim(), biscuit_auth::builder::Algorithm::Ed25519)
            {
                Ok(key) => {
                    return TokenService {
                        root: KeyPair::from(&key),
                    }
                }
                Err(e) => eprintln!(
                    "wk: ignoring malformed token key {} ({e}); generating a new one",
                    key_path.display()
                ),
            }
        }
        let svc = TokenService::new();
        if let Err(e) = std::fs::write(key_path, svc.root.private().to_bytes_hex()) {
            eprintln!(
                "wk: could not persist the token key to {} ({e}); \
                 tokens will not survive a restart",
                key_path.display()
            );
        }
        svc
    }

    /// Mint the base **node** token: the default capability policy a running
    /// node holds ([`NODE_BASE_RULE`] in the authority block). A holder can
    /// attenuate it offline (append checks); the token service can mint a
    /// different authority to change how access works entirely.
    pub fn mint_node_base(&self) -> Result<Vec<u8>, String> {
        Biscuit::builder()
            .code(NODE_BASE_RULE)
            .map_err(|e| format!("biscuit rule: {e}"))?
            .build(&self.root)
            .map_err(|e| format!("biscuit build: {e}"))?
            .to_vec()
            .map_err(|e| format!("biscuit serialize: {e}"))
    }

    /// Mint a token whose authority block is exactly the given Datalog —
    /// crafted tokens for `wk token mint`: custom rules (a different access
    /// logic), `right(resource, action)` facts (API/command grants), checks,
    /// or any mix. The caller writes the policy; this only signs it.
    pub fn mint_code(&self, datalog: &str) -> Result<Vec<u8>, String> {
        Biscuit::builder()
            .code(datalog)
            .map_err(|e| format!("biscuit datalog: {e}"))?
            .build(&self.root)
            .map_err(|e| format!("biscuit build: {e}"))?
            .to_vec()
            .map_err(|e| format!("biscuit serialize: {e}"))
    }

    /// The public key a verifier (the server) needs. Safe to copy anywhere; it
    /// cannot mint or attenuate tokens.
    pub fn public_key(&self) -> PublicKey {
        self.root.public()
    }

    /// Mint a token granting exactly the given `right(resource, action)` pairs,
    /// serialized for transport. This is the credential a client stores and
    /// presents with every command.
    pub fn mint(&self, rights: &[(ResourceKind, Action)]) -> Result<Vec<u8>, String> {
        let mut builder = Biscuit::builder();
        for (res, act) in rights {
            // Both strs come from fixed enums, so there is no injection risk.
            builder = builder
                .fact(format!(r#"right("{}", "{}")"#, res.as_str(), act.as_str()).as_str())
                .map_err(|e| format!("biscuit fact: {e}"))?;
        }
        let token = builder
            .build(&self.root)
            .map_err(|e| format!("biscuit build: {e}"))?;
        token
            .to_vec()
            .map_err(|e| format!("biscuit serialize: {e}"))
    }

    /// Mint a full-authority token (every action on every resource) — what the
    /// trusted local client is handed.
    pub fn mint_admin(&self) -> Result<Vec<u8>, String> {
        let all: Vec<(ResourceKind, Action)> = ResourceKind::ALL
            .iter()
            .flat_map(|&r| Action::ALL.iter().map(move |&a| (r, a)))
            .collect();
        self.mint(&all)
    }
}

impl Default for TokenService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_token_verifies_against_the_public_key() {
        let svc = TokenService::new();
        let token = svc
            .mint(&[
                (ResourceKind::Node, Action::Create),
                (ResourceKind::Wire, Action::Create),
            ])
            .unwrap();
        // The server side verifies by deserializing with the public key.
        assert!(Biscuit::from(&token, svc.public_key()).is_ok());
    }

    #[test]
    fn a_different_services_key_does_not_verify() {
        let svc = TokenService::new();
        let other = TokenService::new();
        let token = svc.mint_admin().unwrap();
        assert!(Biscuit::from(&token, other.public_key()).is_err());
    }

    #[test]
    fn node_base_token_verifies_and_carries_the_wiring_rule() {
        let svc = TokenService::new();
        let token = svc.mint_node_base().unwrap();
        let parsed = Biscuit::from(&token, svc.public_key()).expect("verifies");
        let source = parsed.print_block_source(0).expect("authority block");
        assert!(
            source.contains("can_use") && source.contains("wired"),
            "authority block carries the wiring rule, got: {source}"
        );
    }

    #[test]
    fn persisted_key_round_trips_and_replaces_garbage() {
        let dir = std::env::temp_dir().join("wk-token-key-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("workspace.wk.key");

        // First load creates and persists; second load reuses the same root, so
        // a token minted by the first verifies against the second's key.
        let a = TokenService::load_or_create(&key_path);
        let token = a.mint_node_base().unwrap();
        let b = TokenService::load_or_create(&key_path);
        assert!(Biscuit::from(&token, b.public_key()).is_ok());

        // A corrupted key file is replaced, not fatal — and the old token no
        // longer verifies against the fresh root.
        std::fs::write(&key_path, "not hex").unwrap();
        let c = TokenService::load_or_create(&key_path);
        assert!(Biscuit::from(&token, c.public_key()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
