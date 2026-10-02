//! rUDP record protection core — sans-IO, deterministic.
//!
//! The design is docs/RUDP-SECURITY.md; this module is its crypto core
//! (round x1). B5a wired it into rUDP (`crate::udp`, module `sealed`):
//! the handshake rides the cookie proof/accept, the record layer wraps
//! every session datagram of a sealed door. This module stays sans-IO.
//!
//! - **Handshake** ([`Initiator`], [`Msg1`]): Noise
//!   `NK_25519_ChaChaPoly_BLAKE2s` via `snow` (RustCrypto only, no
//!   `ring`). Message 1 rides the cookie proof, message 2 the accept, so
//!   the existing two round trips stay two. The server's DH runs only
//!   after the caller's cookie check ([`Msg1::cookie_verified`]).
//! - **Record layer** ([`Sealer`], [`Opener`]): ChaCha20-Poly1305 with
//!   one key per direction, the 64-bit counter as nonce, the header as
//!   associated data, a [`REPLAY_WINDOW`]-wide replay window, and key
//!   phases (Noise REKEY) with a grace for the previous phase. Every
//!   refused datagram is one [`Refusal`] variant, so each loss is counted
//!   under its exact name.
//! - **Stateless reset** ([`ResetKey`], [`ResetToken`], [`reset_datagram`]):
//!   `HMAC-BLAKE2s(key, cid)` tokens from a key that survives restarts,
//!   and the reset datagram's layout (B5b).
//! - **Wire layout** ([`wire`]): the SEALED header, encode/decode only.
//!
//! The two halves of a [`Session`] share nothing: the [`Sealer`] goes to
//! the writer, the [`Opener`] to the receive path. No locks, no `unsafe`
//! in this module (RustCrypto's internal SIMD is the dependencies' own).

mod handshake;
mod identity;
mod key;
mod opener;
mod replay;
mod reset;
mod sealer;
pub mod wire;

pub use handshake::{Initiator, Msg1, Responded, Session};
pub use identity::{
    ACCEPT_LEN, Accept, HANDSHAKE_HASH_LEN, HandshakeError, KEY_LEN, MSG1_LEN_MAX, MSG1_LEN_MIN,
    MSG1_PAYLOAD_MAX, MSG2_LEN, NOISE_PATTERN, PROLOGUE_LABEL, StaticKey,
};
pub use opener::{INTEGRITY_LIMIT, Opened, Opener, Refusal};
pub use replay::REPLAY_WINDOW;
pub use reset::{
    RESET_KEY_LEN, RESET_LEN_MAX, RESET_LEN_MIN, RESET_TOKEN_LEN, ResetKey, ResetToken,
    reset_datagram, reset_tail,
};
pub use sealer::{REKEY_MIN_DISTANCE, SEAL_LIMIT, SealError, Sealer};

#[cfg(test)]
mod tests;
