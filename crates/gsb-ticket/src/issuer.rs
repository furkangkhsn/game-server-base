//! The issuer side: mint a ticket from claims with the issuer's key —
//! for a Rust platform, and for tests. A platform in another language
//! mints the same token with its own PASETO v4.public library (or 20
//! lines over its Ed25519; docs/TICKETS.md has a TypeScript example).

use serde::Serialize;

use crate::claims::{self, Claims};
use crate::keys::{IssuerKey, TrustedKey};
use crate::paseto;

/// Why a ticket could not be minted.
#[derive(Debug)]
pub enum MintError {
    /// The game's claims did not serialize to JSON.
    Claims(serde_json::Error),
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claims(e) => write!(f, "the claims do not serialize: {e}"),
        }
    }
}

impl std::error::Error for MintError {}

/// Mints tickets under one signing key; the key id rides the footer.
#[derive(Debug)]
pub struct Issuer {
    key: IssuerKey,
}

impl Issuer {
    /// An issuer signing with `key`.
    pub fn new(key: IssuerKey) -> Self {
        Self { key }
    }

    /// The public key validators must trust for this issuer's tickets.
    pub fn trusted(&self) -> TrustedKey {
        self.key.trusted()
    }

    /// The token for `claims`: `v4.public.<claims ‖ signature>.<{"kid":…}>`.
    pub fn mint<T: Serialize>(&self, claims: &Claims<T>) -> Result<String, MintError> {
        let message = claims::encode(claims).map_err(MintError::Claims)?;
        Ok(paseto::sign(
            &self.key.key,
            &message,
            &footer(&self.key.kid),
            b"",
        ))
    }
}

/// The footer naming `kid` (key ids are `[A-Za-z0-9._-]`: no escaping).
pub(crate) fn footer(kid: &str) -> Vec<u8> {
    format!(r#"{{"kid":"{kid}"}}"#).into_bytes()
}
