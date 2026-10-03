//! The keys: an issuer's Ed25519 signing key (secret, the lobby's) and
//! the set of trusted issuer PUBLIC keys a validator holds, each under
//! its key id (`kid`) — the rotation story (docs/TICKETS.md): a new key
//! is added to every validator first, the lobby then signs with it, the
//! old key is removed once its last ticket has expired.
//!
//! **Never echoed:** a parse error names what is wrong (a length, a
//! position, the kid), never a character of a key; `Debug` of a signing
//! key is redacted, and the secret is wiped when dropped.

use ed25519_dalek::{SigningKey, VerifyingKey};

/// The longest key id, in bytes.
pub const KID_MAX: usize = 64;

/// Why a key or key id was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError(pub String);

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeyError {}

/// A key id: 1–64 bytes of `[A-Za-z0-9._-]` (it travels in the token's
/// footer and in config files; nothing that needs escaping).
pub fn check_kid(kid: &str) -> Result<(), KeyError> {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    if kid.is_empty() || kid.len() > KID_MAX || !kid.chars().all(ok) {
        return Err(KeyError(format!(
            "a key id is 1-{KID_MAX} characters of [A-Za-z0-9._-]"
        )));
    }
    Ok(())
}

/// 64 hex characters into 32 bytes; the error never carries a
/// character of the input.
pub fn parse_hex32(s: &str) -> Result<zeroize::Zeroizing<[u8; 32]>, KeyError> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return Err(KeyError(format!(
            "expected 64 hex characters, got {} characters",
            s.chars().count()
        )));
    }
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| KeyError(format!("a non-hex character at position {}", i * 2)))?;
    }
    Ok(out)
}

/// Lowercase hex of 32 bytes.
pub fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// An issuer's public key under its key id: what a validator trusts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
    pub(crate) kid: String,
    pub(crate) key: VerifyingKey,
}

impl TrustedKey {
    /// `kid` → the Ed25519 public key `public`. A weak (small-order)
    /// key is refused here, not at the first ticket.
    pub fn new(kid: impl Into<String>, public: [u8; 32]) -> Result<Self, KeyError> {
        let kid = kid.into();
        check_kid(&kid)?;
        let key = VerifyingKey::from_bytes(&public)
            .map_err(|_| KeyError(format!("key `{kid}`: not an Ed25519 public key")))?;
        if key.is_weak() {
            return Err(KeyError(format!("key `{kid}`: a weak (small-order) key")));
        }
        Ok(Self { kid, key })
    }

    /// The config spelling `kid:<64 hex characters>`.
    pub fn parse(spec: &str) -> Result<Self, KeyError> {
        let (kid, hex) = spec
            .split_once(':')
            .ok_or_else(|| KeyError("an issuer key is `kid:<64 hex characters>`".into()))?;
        let bytes = parse_hex32(hex).map_err(|e| KeyError(format!("key `{kid}`: {e}")))?;
        Self::new(kid, *bytes)
    }

    /// The key id.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The public key's bytes.
    pub fn public(&self) -> [u8; 32] {
        self.key.to_bytes()
    }
}

/// An issuer's signing key under its key id (the lobby's secret).
pub struct IssuerKey {
    pub(crate) kid: String,
    pub(crate) key: SigningKey,
}

impl IssuerKey {
    /// The key from its 32-byte seed (the RFC 8032 private key).
    pub fn from_seed(kid: impl Into<String>, seed: [u8; 32]) -> Result<Self, KeyError> {
        let mut seed = zeroize::Zeroizing::new(seed);
        let kid = kid.into();
        check_kid(&kid)?;
        let key = SigningKey::from_bytes(&seed);
        zeroize::Zeroize::zeroize(&mut *seed);
        Ok(Self { kid, key })
    }

    /// A fresh key from the OS entropy source (`None`: it failed).
    pub fn generate(kid: impl Into<String>) -> Option<Self> {
        let mut seed = zeroize::Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut *seed).ok()?;
        Self::from_seed(kid, *seed).ok()
    }

    /// The seed, as 64 hex characters — for a config or secret store, by
    /// the caller's explicit choice; never logged here.
    pub fn seed_hex(&self) -> zeroize::Zeroizing<String> {
        zeroize::Zeroizing::new(hex32(&self.key.to_bytes()))
    }

    /// The public half under the same key id: what validators trust.
    pub fn trusted(&self) -> TrustedKey {
        TrustedKey {
            kid: self.kid.clone(),
            key: self.key.verifying_key(),
        }
    }

    /// The key id.
    pub fn kid(&self) -> &str {
        &self.kid
    }
}

/// Redacted: the key id and the public key only.
impl std::fmt::Debug for IssuerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuerKey")
            .field("kid", &self.kid)
            .field("public", &hex32(&self.key.verifying_key().to_bytes()))
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_secret() {
        let seed = [0x5au8; 32];
        let key = IssuerKey::from_seed("k1", seed).expect("valid");
        let shown = format!("{key:?}");
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(!shown.contains(&hex32(&seed)), "{shown}");
        assert!(!shown.contains("5a5a"), "{shown}");
    }

    #[test]
    fn a_parse_error_never_echoes_the_key() {
        let secretish = "zz".repeat(32);
        let e = TrustedKey::parse(&format!("k1:{secretish}")).expect_err("not hex");
        assert!(!e.0.contains("zz"), "{e}");
        assert!(e.0.contains("position 0"), "{e}");
        let e = TrustedKey::parse("k1:abcd").expect_err("short");
        assert!(!e.0.contains("abcd"), "{e}");
        assert!(TrustedKey::parse("no-colon").is_err());
        assert!(TrustedKey::parse(&format!("bad kid:{}", "00".repeat(32))).is_err());
    }

    #[test]
    fn a_weak_key_is_refused_up_front() {
        // The identity point: small order, would accept forged signatures
        // under a non-strict verify.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(TrustedKey::new("k1", identity).is_err());
        let real = IssuerKey::from_seed("k1", [7u8; 32])
            .expect("valid")
            .trusted();
        assert!(TrustedKey::new("k1", real.public()).is_ok());
    }
}
