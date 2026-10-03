//! A validator from configuration — the server's `[ticket]` table
//! (gsb-server's `ticket` feature) or any other TOML/JSON source:
//!
//! ```toml
//! [ticket]
//! issuer_keys = ["lobby-2026-10:<64 hex: Ed25519 public key>"]
//! audience = "eu-1"
//! max_skew_secs = 30        # default 30
//! max_lifetime_secs = 900   # default 900
//! single_use = false        # default false (reusable within expiry)
//! replay_capacity = 100000  # single_use only
//! timeout_ms = 2000         # the hook's deadline
//! ```
//!
//! An unknown key refuses (like every config table of the engine); an
//! error never carries a character of a key. A validator built from
//! config types no game claims: the game's `ext` passes through as
//! verified bytes ([`gsb_core::auth::ValidatedTicket::extra`]) and no
//! game check runs — a game that needs one builds its validator in code
//! ([`crate::Validator::with_check`]).

use std::time::Duration;

use gsb_core::auth::TicketAuth;
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};

use crate::keys::{KeyError, TrustedKey};
use crate::replay::ReplayGuard;
use crate::validator::{DEFAULT_MAX_LIFETIME_SECS, DEFAULT_SKEW_SECS, Validator};

/// The `[ticket]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorConfig {
    /// The trusted issuer public keys, `kid:<64 hex>` each.
    pub issuer_keys: Vec<String>,
    /// The audience tickets must name (`aud`).
    pub audience: String,
    #[serde(default = "default_skew")]
    pub max_skew_secs: u64,
    #[serde(default = "default_lifetime")]
    pub max_lifetime_secs: u64,
    #[serde(default)]
    pub single_use: bool,
    #[serde(default = "default_capacity")]
    pub replay_capacity: usize,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_skew() -> u64 {
    DEFAULT_SKEW_SECS as u64
}
fn default_lifetime() -> u64 {
    DEFAULT_MAX_LIFETIME_SECS as u64
}
fn default_capacity() -> usize {
    100_000
}
fn default_timeout() -> u64 {
    2_000
}

impl ValidatorConfig {
    /// The validator this table describes, with the game's claims typed
    /// as `T`. `single_use` spawns the guard (inside a tokio runtime).
    pub fn build<T: DeserializeOwned>(&self) -> Result<Validator<T>, KeyError> {
        let keys = self
            .issuer_keys
            .iter()
            .map(|k| TrustedKey::parse(k))
            .collect::<Result<Vec<_>, _>>()?;
        if self.timeout_ms == 0 {
            return Err(KeyError("timeout_ms = 0 would refuse every ticket".into()));
        }
        let secs = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        let mut v = Validator::new(self.audience.clone(), keys)?
            .with_skew(secs(self.max_skew_secs))
            .with_max_lifetime(secs(self.max_lifetime_secs));
        if self.single_use {
            if self.replay_capacity == 0 {
                return Err(KeyError("single_use needs replay_capacity > 0".into()));
            }
            v = v.single_use(ReplayGuard::spawn(self.replay_capacity));
        }
        Ok(v)
    }

    /// The engine's hook, the game's claims passed through untyped.
    pub fn auth(&self) -> Result<TicketAuth, KeyError> {
        let timeout = Duration::from_millis(self.timeout_ms);
        Ok(self.build::<IgnoredAny>()?.into_auth(timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Claims, Issuer, IssuerKey};

    fn table(extra: &str) -> Result<ValidatorConfig, serde_json::Error> {
        let key = IssuerKey::from_seed("lobby-1", [3; 32])
            .expect("valid")
            .trusted();
        let hex = crate::keys::hex32(&key.public());
        serde_json::from_str(&format!(
            r#"{{"issuer_keys":["lobby-1:{hex}"],"audience":"eu-1"{extra}}}"#
        ))
    }

    #[tokio::test]
    async fn a_table_builds_the_validator_it_describes() {
        let cfg = table(r#","single_use":true,"max_skew_secs":5"#).expect("parses");
        assert_eq!((cfg.max_lifetime_secs, cfg.timeout_ms), (900, 2_000));
        let v = cfg.build::<IgnoredAny>().expect("builds");
        let issuer = Issuer::new(IssuerKey::from_seed("lobby-1", [3; 32]).expect("valid"));
        let c = Claims::new("ann", 1, "eu-1", 1_800_000_000, 60, ()).expect("entropy");
        let t = issuer.mint(&c).expect("mint");
        assert!(v.validate_at(t.as_bytes(), 1_800_000_000).await.is_ok());
        let again = v.validate_at(t.as_bytes(), 1_800_000_000).await;
        assert!(again.is_err(), "single_use");
        assert!(cfg.auth().is_ok());
    }

    #[test]
    fn an_unknown_key_or_a_bad_value_refuses_without_echoing_a_key() {
        let e = table(r#","max_skew":5"#).expect_err("unknown key");
        assert!(e.to_string().contains("max_skew"), "{e}");
        let bad: ValidatorConfig = serde_json::from_str(
            r#"{"issuer_keys":["k1:00112233445566778899aabbccddeeffXX"],"audience":"a"}"#,
        )
        .expect("parses");
        let e = bad.build::<IgnoredAny>().err().expect("refused");
        assert!(!e.0.contains("0011") && !e.0.contains("XX"), "{e}");
        let zero = table(r#","timeout_ms":0"#).expect("parses");
        assert!(zero.build::<IgnoredAny>().is_err());
    }
}
