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

use biscuit_auth::{Biscuit, KeyPair};

pub use biscuit_auth::PublicKey;
pub use wk_protocol::{Action, ResourceKind};

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
}
