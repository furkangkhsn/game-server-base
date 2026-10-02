//! The Noise suite, the server's static key, the accept payload, errors.

use snow::params::{DHChoice, NoiseParams};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use zeroize::{Zeroize, Zeroizing};

use super::reset::{RESET_TOKEN_LEN, ResetToken};

/// The Noise protocol name, fixed. NK: the client knows the server's
/// static key up front (pinned from the platform), the client itself is
/// anonymous at this layer (it proves itself with the AUTH ticket inside
/// the sealed channel). BLAKE2s over SHA-256: WireGuard's choice, fast in
/// plain software on 32- and 64-bit CPUs without SHA extensions (mobile,
/// a future C#/Unity port via BouncyCastle), and snow's resolver has it
/// without `ring`.
pub const NOISE_PATTERN: &str = "Noise_NK_25519_ChaChaPoly_BLAKE2s";
/// Prepended to the caller's context to form the Noise prologue.
pub const PROLOGUE_LABEL: &[u8] = b"gsb-rudp-seal/1\0";
/// Bytes of an X25519 key.
pub const KEY_LEN: usize = 32;
/// Bytes of the handshake hash (channel binding).
pub const HANDSHAKE_HASH_LEN: usize = 32;
/// Most payload bytes message 1 may carry. It is encrypted to the server
/// key but REPLAYABLE and without forward secrecy (Noise §7.7 for NK
/// message 1): never put a secret such as the ticket in it.
pub const MSG1_PAYLOAD_MAX: usize = 32;
/// Message 1 without payload: ephemeral key + payload tag.
pub const MSG1_LEN_MIN: usize = KEY_LEN + 16;
/// Message 1 with the largest payload.
pub const MSG1_LEN_MAX: usize = MSG1_LEN_MIN + MSG1_PAYLOAD_MAX;
/// Bytes of the accept payload: CID + reset token.
pub const ACCEPT_LEN: usize = 8 + RESET_TOKEN_LEN;
/// Message 2, always this exact size: ephemeral key + accept + tag.
pub const MSG2_LEN: usize = KEY_LEN + ACCEPT_LEN + 16;

/// Why a handshake step failed. Each is a distinct, countable name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeError {
    /// Wrong message length or shape; refused before any DH.
    Malformed,
    /// The caller's message-1 payload is over [`MSG1_PAYLOAD_MAX`].
    PayloadTooLarge,
    /// Authentication failed: a forged message, a client pinning another
    /// server key, or a context (cookie) mismatch.
    Decrypt,
    /// The crypto backend failed (RNG, a state misuse such as finishing
    /// twice); not caused by the peer's bytes.
    Internal,
}

pub(super) fn map_snow(e: snow::Error) -> HandshakeError {
    match e {
        snow::Error::Decrypt => HandshakeError::Decrypt,
        snow::Error::Input => HandshakeError::Malformed,
        _ => HandshakeError::Internal,
    }
}

pub(super) fn params() -> NoiseParams {
    NOISE_PATTERN
        .parse()
        .expect("the fixed pattern name parses")
}

pub(super) fn prologue(context: &[u8]) -> Vec<u8> {
    [PROLOGUE_LABEL, context].concat()
}

/// The server's static X25519 key: its identity. The platform hands the
/// public half out with the ticket and address; the client pins it.
pub struct StaticKey {
    private: Zeroizing<[u8; KEY_LEN]>,
    public: [u8; KEY_LEN],
}

impl StaticKey {
    /// From configured private key bytes (the server reads them from its
    /// config since B5a — `udp_static_key`; a missing key is a startup
    /// error there, never a plaintext fallback).
    pub fn from_private(private: [u8; KEY_LEN]) -> Result<Self, HandshakeError> {
        let mut dh = DefaultResolver
            .resolve_dh(&DHChoice::Curve25519)
            .ok_or(HandshakeError::Internal)?;
        dh.set(&private);
        let public = dh
            .pubkey()
            .try_into()
            .map_err(|_| HandshakeError::Internal)?;
        Ok(StaticKey {
            private: Zeroizing::new(private),
            public,
        })
    }

    /// A fresh key from the OS RNG (tooling and tests).
    pub fn generate() -> Result<Self, HandshakeError> {
        let mut pair = snow::Builder::new(params())
            .generate_keypair()
            .map_err(map_snow)?;
        let private = pair.private[..]
            .try_into()
            .map_err(|_| HandshakeError::Internal);
        pair.private.zeroize();
        Self::from_private(private?)
    }

    /// The public half, to publish and pin.
    pub fn public(&self) -> [u8; KEY_LEN] {
        self.public
    }

    /// The private half, wiped when the returned value drops: to write a
    /// key file or an ephemeral test/load-run configuration. Never log it
    /// (this type's `Debug` prints the public half only).
    pub fn private_bytes(&self) -> Zeroizing<[u8; KEY_LEN]> {
        Zeroizing::new(*self.private)
    }

    pub(super) fn private(&self) -> &[u8] {
        &self.private[..]
    }
}

/// Only the public half: a `Debug` that printed the private key would be
/// a leak waiting for a log line.
impl std::fmt::Debug for StaticKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticKey")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

/// What the server tells the client inside message 2 (encrypted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accept {
    /// The session's wire CID (random, server-chosen; not `ConnectionId`).
    pub cid: u64,
    /// The token that ends this session statelessly after a restart.
    pub reset_token: ResetToken,
}

impl Accept {
    /// `cid u64 LE || token 16`.
    pub fn encode(&self) -> [u8; ACCEPT_LEN] {
        let mut b = [0u8; ACCEPT_LEN];
        b[..8].copy_from_slice(&self.cid.to_le_bytes());
        b[8..].copy_from_slice(&self.reset_token.to_bytes());
        b
    }

    /// Exact-length decode.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let b: &[u8; ACCEPT_LEN] = b.try_into().ok()?;
        let (cid, token) = b.split_at(8);
        Some(Accept {
            cid: u64::from_le_bytes(cid.try_into().ok()?),
            reset_token: ResetToken::from_bytes(token.try_into().ok()?),
        })
    }
}
