//! Stateless reset token: `HMAC-BLAKE2s(reset_key, label || cid)[..16]`.
//!
//! The reset key lives in the server config and survives restarts. The
//! server hands each session its token inside the encrypted accept
//! (handshake message 2). After a restart the server has lost the session
//! but not the key: it answers a datagram with an unknown CID with that
//! CID's token, and the client — comparing in constant time against the
//! token it holds — ends the session at once instead of waiting out the
//! REL deadline (QUIC's stateless reset, RFC 9000 §10.3). BLAKE2s keeps
//! one hash function across the protocol (the Noise suite's).

use blake2::Blake2s256;
use hmac::{Mac, SimpleHmac};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// Bytes of the server's reset key.
pub const RESET_KEY_LEN: usize = 32;
/// Bytes of a reset token (QUIC's size).
pub const RESET_TOKEN_LEN: usize = 16;
/// Domain separation: this key derives nothing else.
const LABEL: &[u8] = b"gsb-rudp-reset/1";

/// The server's stateless reset key (from config; wiped on drop).
pub struct ResetKey(Zeroizing<[u8; RESET_KEY_LEN]>);

impl ResetKey {
    /// Wraps configured key bytes (B5b reads them from the config).
    pub fn from_bytes(key: [u8; RESET_KEY_LEN]) -> Self {
        ResetKey(Zeroizing::new(key))
    }

    /// The token of `cid`. Same key and CID give the same token on every
    /// boot; nothing else is needed to answer an unknown CID.
    pub fn token(&self, cid: u64) -> ResetToken {
        let mut mac = <SimpleHmac<Blake2s256> as Mac>::new_from_slice(&self.0[..])
            .expect("HMAC accepts a key of any length");
        mac.update(LABEL);
        mac.update(&cid.to_le_bytes());
        let full = mac.finalize().into_bytes();
        let mut t = [0u8; RESET_TOKEN_LEN];
        t.copy_from_slice(&full[..RESET_TOKEN_LEN]);
        ResetToken(t)
    }
}

/// A session's reset token. Equality is constant-time; `Debug` hides it
/// (anyone holding it can end the session).
#[derive(Clone, Copy)]
pub struct ResetToken([u8; RESET_TOKEN_LEN]);

impl ResetToken {
    /// Wraps token bytes (the client side, from the accept).
    pub fn from_bytes(token: [u8; RESET_TOKEN_LEN]) -> Self {
        ResetToken(token)
    }

    /// The token bytes, to put on the wire.
    pub fn to_bytes(&self) -> [u8; RESET_TOKEN_LEN] {
        self.0
    }

    /// Constant-time check of a received candidate (e.g. the last 16 bytes
    /// of an unknown datagram). A wrong length is simply `false`.
    pub fn matches(&self, candidate: &[u8]) -> bool {
        candidate.len() == RESET_TOKEN_LEN && bool::from(self.0[..].ct_eq(candidate))
    }
}

impl PartialEq for ResetToken {
    fn eq(&self, other: &Self) -> bool {
        self.matches(&other.0)
    }
}

impl Eq for ResetToken {}

impl std::fmt::Debug for ResetToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResetToken(..)")
    }
}
