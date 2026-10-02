//! The send half: owns the outgoing key and the counter.

use super::key::PhaseKey;
use super::replay::REPLAY_WINDOW;
use super::wire::{Header, KeyPhase};

/// The counter a sealer never reaches. The nonce space is 2^64, Noise
/// reserves 2^64-1 for REKEY, and QUIC stops its packet numbers at 2^62
/// (RFC 9000 §12.3); ChaCha20-Poly1305 has no practical confidentiality
/// limit below that (RFC 9001 §6.6). At a million datagrams per second
/// 2^62 lasts ~146,000 years: the point is a hard refusal, never a wrap.
pub const SEAL_LIMIT: u64 = 1 << 62;

/// Datagrams a key generation must seal before the next rekey, so every
/// datagram two generations old is already outside the peer's replay
/// window (the opener keeps only one previous key).
pub const REKEY_MIN_DISTANCE: u64 = REPLAY_WINDOW;

/// Why a seal or a rekey was refused. Each is a distinct, countable name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// The counter reached [`SEAL_LIMIT`]: the session must end (or
    /// handshake again); nothing more can be sealed under it.
    CounterExhausted,
    /// Rekey asked fewer than [`REKEY_MIN_DISTANCE`] datagrams after the
    /// previous one.
    RekeyTooSoon,
    /// Rekey asked before the peer acknowledged a datagram of the current
    /// key phase (RFC 9001 §6.1's rule): the peer may not have the
    /// current key yet, and it keeps only one key back.
    RekeyUnconfirmed,
}

/// Seals datagrams in one direction. Lives in the writer; shares nothing
/// with the [`Opener`](super::Opener).
pub struct Sealer {
    key: PhaseKey,
    cid: Option<u64>,
    next: u64,
    generation: u64,
    phase_start: u64,
    confirmed: bool,
}

impl Sealer {
    /// `cid` is `Some` for the client's c→s sealer, `None` for the server.
    pub(super) fn new(raw: &mut [u8; 32], cid: Option<u64>) -> Self {
        Sealer {
            key: PhaseKey::new(raw),
            cid,
            next: 0,
            generation: 0,
            phase_start: 0,
            confirmed: false,
        }
    }

    /// Appends one whole SEALED datagram (header, ciphertext, tag) to
    /// `out` and returns the counter it used. The header this call writes
    /// is the AEAD's associated data, so header and nonce cannot disagree.
    pub fn seal(&mut self, plaintext: &[u8], out: &mut Vec<u8>) -> Result<u64, SealError> {
        if self.next >= SEAL_LIMIT {
            return Err(SealError::CounterExhausted);
        }
        let counter = self.next;
        let header = Header {
            phase: self.phase(),
            cid: self.cid,
            counter,
        };
        let (h, len) = header.encode();
        out.extend_from_slice(&h[..len]);
        self.key.seal(counter, &h[..len], plaintext, out);
        self.next += 1;
        Ok(counter)
    }

    /// The peer acknowledged the datagram sealed at `counter` (the REL
    /// layer maps its ACK to the counter it sent). One acknowledged
    /// datagram of the current phase confirms the peer holds the key.
    pub fn note_peer_ack(&mut self, counter: u64) {
        if counter >= self.phase_start && counter < self.next {
            self.confirmed = true;
        }
    }

    /// Switches to the next key generation; later datagrams carry the
    /// flipped phase bit. WHEN to rekey is policy (B5b: `crate::udp`'s
    /// `RekeyPolicy`); this only refuses a rekey the peer's opener could
    /// not follow.
    pub fn rekey(&mut self) -> Result<(), SealError> {
        if self.next - self.phase_start < REKEY_MIN_DISTANCE {
            return Err(SealError::RekeyTooSoon);
        }
        if !self.confirmed {
            return Err(SealError::RekeyUnconfirmed);
        }
        self.key = self.key.rekey();
        self.generation += 1;
        self.phase_start = self.next;
        self.confirmed = false;
        Ok(())
    }

    /// The counter the next [`seal`](Self::seal) will use.
    pub fn next_counter(&self) -> u64 {
        self.next
    }

    /// The current key generation (0 after the handshake).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The current key phase bit.
    pub fn phase(&self) -> KeyPhase {
        KeyPhase::of(self.generation)
    }

    #[cfg(test)]
    pub(crate) fn set_next_counter_for_test(&mut self, next: u64) {
        self.next = next;
    }
}
