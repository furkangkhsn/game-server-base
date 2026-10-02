//! Stateless reset token: `HMAC-BLAKE2s(reset_key, label || cid)[..16]`.
//!
//! The reset key survives restarts (B5b: the server config's
//! `udp_reset_key`, or derived from the static key — [`ResetKey::derived_from`]
//! — and bound to its door, [`ResetKey::for_door`]). The server hands each
//! session its token inside the encrypted accept (handshake message 2).
//! After a restart the server has lost the session but not the key: it
//! answers a datagram with an unknown CID with that CID's token in a
//! [`reset_datagram`], and the client — comparing in constant time
//! against the token it holds — ends the session at once instead of
//! waiting out the REL deadline (QUIC's stateless reset, RFC 9000 §10.3).
//! BLAKE2s keeps one hash function across the protocol (the Noise
//! suite's).

use blake2::Blake2s256;
use hmac::{Mac, SimpleHmac};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use super::StaticKey;
use super::wire::{HEADER_LEN_S2C, KIND_PHASE_BIT, KIND_SEALED, OVERHEAD_S2C};

mod datagram;
pub use datagram::{RESET_LEN_MAX, RESET_LEN_MIN, reset_datagram, reset_tail};

/// Bytes of the server's reset key.
pub const RESET_KEY_LEN: usize = 32;
/// Bytes of a reset token (QUIC's size).
pub const RESET_TOKEN_LEN: usize = 16;
/// Domain separation: this key derives nothing else.
const LABEL: &[u8] = b"gsb-rudp-reset/1";
/// The derivation of a reset key from the static key (B5b): a label of
/// its own, so the derived key is unrelated to every other use of the
/// static key (the Diffie-Hellman never sees it).
const LABEL_FROM_STATIC: &[u8] = b"gsb-rudp-reset-key/1";
/// The derivation of one door's key: two doors of one server never
/// answer with each other's tokens (a door that does not know a CID must
/// not reveal the token of another door's live session).
const LABEL_DOOR: &[u8] = b"gsb-rudp-reset-door/1";

/// `HMAC-BLAKE2s(key, parts..)`, whole.
fn mac(key: &[u8], parts: &[&[u8]]) -> Zeroizing<[u8; RESET_KEY_LEN]> {
    let mut mac = <SimpleHmac<Blake2s256> as Mac>::new_from_slice(key)
        .expect("HMAC accepts a key of any length");
    for p in parts {
        mac.update(p);
    }
    let mut out = Zeroizing::new([0u8; RESET_KEY_LEN]);
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

/// The server's stateless reset key (from config; wiped on drop).
pub struct ResetKey(Zeroizing<[u8; RESET_KEY_LEN]>);

impl ResetKey {
    /// Wraps configured key bytes (the server's `udp_reset_key`).
    pub fn from_bytes(key: [u8; RESET_KEY_LEN]) -> Self {
        ResetKey(Zeroizing::new(key))
    }

    /// The reset key of a server that configures none (B5b):
    /// `HMAC-BLAKE2s(static private key, "gsb-rudp-reset-key/1")`. It
    /// survives every restart the static key survives — always, since
    /// clients pin that key — with no second secret to manage. A PRF
    /// output under a label of its own: it reveals nothing of the static
    /// key, and the static key's Diffie-Hellman never sees it.
    pub fn derived_from(key: &StaticKey) -> Self {
        ResetKey(mac(key.private(), &[LABEL_FROM_STATIC]))
    }

    /// This key bound to one door (`door`: the door's bound address,
    /// encoded by the caller): `HMAC-BLAKE2s(key, "gsb-rudp-reset-door/1"
    /// || door)`. The same door after a restart derives the same key; two
    /// doors of one server never share a token.
    pub fn for_door(&self, door: &[u8]) -> Self {
        ResetKey(mac(&self.0[..], &[LABEL_DOOR, door]))
    }

    /// The token of `cid`. Same key and CID give the same token on every
    /// boot; nothing else is needed to answer an unknown CID.
    pub fn token(&self, cid: u64) -> ResetToken {
        let full = mac(&self.0[..], &[LABEL, &cid.to_le_bytes()]);
        let mut t = [0u8; RESET_TOKEN_LEN];
        t.copy_from_slice(&full[..RESET_TOKEN_LEN]);
        ResetToken(t)
    }
}

/// Never the key.
impl std::fmt::Debug for ResetKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResetKey(..)")
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
