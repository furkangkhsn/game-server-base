//! One direction's AEAD key for one key generation.
//!
//! The nonce is the Noise ChaChaPoly encoding of the record counter: four
//! zero bytes, then the counter as u64 little-endian (Noise §12.3 /
//! RFC 8439 §2.8). The next generation's key is Noise's
//! `REKEY(k) = ENCRYPT(k, 2^64-1, "", zeros(32))[..32]` (Noise §4.2,
//! §11.3): one ChaCha20 block, no extra dependency, one-way (an old key
//! cannot be recomputed from a newer one), and byte-identical to snow's
//! `rekey_*` so a C# port can test against the same vectors.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};
use zeroize::Zeroize;

use super::wire::TAG_LEN;

/// The nonce Noise reserves for REKEY. The record counter stops far below
/// it (`SEAL_LIMIT`), so no datagram ever shares a keystream with a rekey.
const REKEY_NONCE: u64 = u64::MAX;

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n.into()
}

/// A ChaCha20-Poly1305 key; the cipher zeroizes it on drop.
pub(super) struct PhaseKey(ChaCha20Poly1305);

impl PhaseKey {
    /// Builds the key and wipes the caller's copy of the raw bytes.
    pub(super) fn new(raw: &mut [u8; 32]) -> Self {
        let key = ChaCha20Poly1305::new(&(*raw).into());
        raw.zeroize();
        PhaseKey(key)
    }

    /// Appends `ciphertext || tag` of `plaintext` to `out`.
    pub(super) fn seal(&self, counter: u64, aad: &[u8], plaintext: &[u8], out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(plaintext);
        // The only error is a buffer over 2^38 bytes; a datagram is < 64 KiB.
        let tag = self
            .0
            .encrypt_in_place_detached(&nonce(counter), aad, &mut out[start..])
            .expect("a datagram is far below the ChaCha20-Poly1305 length limit");
        out.extend_from_slice(&tag);
    }

    /// Authenticates and decrypts `ciphertext || tag`; `None` on any
    /// authentication failure (the caller checked `body.len() >= TAG_LEN`).
    pub(super) fn open(&self, counter: u64, aad: &[u8], body: &[u8]) -> Option<Vec<u8>> {
        let (ct, tag) = body.split_at(body.len() - TAG_LEN);
        let mut buf = ct.to_vec();
        self.0
            .decrypt_in_place_detached(&nonce(counter), aad, &mut buf, Tag::from_slice(tag))
            .ok()
            .map(|()| buf)
    }

    /// The next generation's key (Noise REKEY).
    pub(super) fn rekey(&self) -> Self {
        let mut raw = [0u8; 32];
        // Discarding the tag is the definition of REKEY.
        let _tag = self
            .0
            .encrypt_in_place_detached(&nonce(REKEY_NONCE), &[], &mut raw)
            .expect("32 bytes is far below the ChaCha20-Poly1305 length limit");
        PhaseKey::new(&mut raw)
    }
}
