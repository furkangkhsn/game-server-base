//! The sealed door's half of a verified proof (B5a, module
//! `crate::udp::sealed`): Noise message 1's shape, the global DH budget
//! (B119), the session's CID, and the responder's Diffie-Hellman — in
//! that order, every refusal under its own name. A child of [`super`],
//! so the demux's state stays private.
//!
//! The order is the rule (RUDP-SECURITY §4): no X25519 work runs before
//! the cookie verified (the caller), the per-source cap admitted (the
//! caller) and the budget gave a token (here). `Msg1::parse` is a
//! length check; the DH is `Msg1::cookie_verified`, reached last.

use tracing::warn;

use crate::seal::{Accept, HandshakeError, Msg1, Opener, Sealer};
use crate::udp::path::draw_u64;
use crate::udp::sealed::{
    PROOF_MSG1_AT, SEALED_PROOF_MAX, SEALED_PROOF_MIN, context, encode_sealed_accept,
};

/// A sealed session's birth: its CID, the two record halves, and the
/// accept datagram (kept for a re-sent proof).
pub(super) struct Noised {
    pub(super) cid: u64,
    pub(super) sealer: Sealer,
    pub(super) opener: Opener,
    pub(super) accept: Box<[u8]>,
}

impl super::Demux {
    /// The key-phase policy a sealed session's writer gets (B5b).
    pub(super) fn door_rekey(&self) -> crate::udp::RekeyPolicy {
        self.seal.as_ref().map(|s| s.rekey).unwrap_or_default()
    }

    /// Whether a verified proof of `n` bytes may go on: always on a
    /// plaintext door; on a sealed one only with a message 1 of a valid
    /// length (a plaintext client's proof, or a malformed one, is
    /// refused here and counted — before the per-source cap, so neither
    /// takes a place).
    pub(super) fn proof_admissible(&mut self, n: usize) -> bool {
        let Some(seal) = self.seal.as_mut() else {
            return true;
        };
        if n <= PROOF_MSG1_AT {
            seal.counts.proofs_refused_plaintext += 1;
            if !std::mem::replace(&mut seal.warned_plaintext, true) {
                warn!(
                    "rUDP: a plaintext client's proof reached this sealed door; refused \
                     (counted as udp_proofs_refused_plaintext; such clients need the \
                     server's public key, or a door with udp_security = \"plaintext\")"
                );
            }
            return false;
        }
        if !(SEALED_PROOF_MIN..=SEALED_PROOF_MAX).contains(&n) {
            seal.counts.handshakes_malformed += 1;
            return false;
        }
        true
    }

    /// The sealed handshake for the verified, admitted proof of `n`
    /// bytes in the demux's buffer: the budget, a CID, then the DH. `None`
    /// — counted under its name — creates nothing and sends nothing (the
    /// client re-sends its proof).
    pub(super) fn noise(&mut self, n: usize, nonce: u64, cookie: u64) -> Option<Noised> {
        let seal = self.seal.as_mut()?;
        if !seal.budget.admits(tokio::time::Instant::now()) {
            return None;
        }
        // The CID is the routing key of every c→s record: a sealed
        // session always has one (no CID, no session — never a weaker
        // value).
        let cid = match draw_u64() {
            Some(cid) if !self.sessions.cid_taken(cid) => cid,
            _ => {
                self.mig.entropy_failed += 1;
                return None;
            }
        };
        let seal = self.seal.as_mut()?;
        let accept = Accept {
            cid,
            reset_token: seal.reset.token(cid),
        };
        let done = Msg1::parse(&self.buf[PROOF_MSG1_AT..n])
            .and_then(|m| m.cookie_verified(&seal.key, &context(nonce, cookie), &accept));
        match done {
            Ok(r) => {
                self.mig.cids_assigned += 1;
                let (sealer, opener) = r.session.into_halves();
                Some(Noised {
                    cid,
                    sealer,
                    opener,
                    accept: encode_sealed_accept(&r.msg2).into_boxed_slice(),
                })
            }
            Err(e) => {
                let c = &mut seal.counts;
                *match e {
                    HandshakeError::Malformed | HandshakeError::PayloadTooLarge => {
                        &mut c.handshakes_malformed
                    }
                    HandshakeError::Decrypt => &mut c.handshakes_failed_decrypt,
                    HandshakeError::Internal => &mut c.handshakes_failed_internal,
                } += 1;
                None
            }
        }
    }
}
