//! The record layer, client side (B5a, module `crate::udp::sealed`): a
//! client that pinned the server's public key
//! ([`UdpClientConfig::server_key`]) runs Noise NK inside the cookie
//! handshake and, once the server's message 2 authenticated, seals every
//! datagram it sends and opens every one it receives. A datagram that
//! does not open is dropped and counted under its `seal_*` name in
//! [`UdpClientStats`]. A child of [`super`], so it reaches the client's
//! private state directly.
//!
//! B5b: the send half rekeys under the client's [`RekeyPolicy`] (module
//! `crate::udp::sealed`, `rekey`), confirmed by the server's ACKs; and a
//! refused datagram of a stateless reset's size whose last 16 bytes are
//! the session's reset token (from message 2; compared in constant time)
//! ends the session at once — the server lost it (a restart) — instead of
//! after the reliable band's 5 s liveness bound.

use super::*;
use crate::seal::{Opener, Refusal, ResetToken, Sealer, reset_tail};
use crate::udp::sealed::SendHalf;

/// The error of a send whose record counter ran out (the session is
/// over: [`UdpClient::is_established`] is `false`).
pub(super) fn exhausted() -> std::io::Error {
    std::io::Error::other("rUDP record layer: the session's record counter is exhausted")
}

/// The client's record state.
#[derive(Default)]
pub(super) struct Seal {
    /// The pinned server key (`None`: a plaintext client).
    pub(super) key: Option<[u8; crate::seal::KEY_LEN]>,
    policy: RekeyPolicy,
    sealer: Option<SendHalf>,
    opener: Option<Opener>,
    /// The session's stateless reset token (message 2's).
    token: Option<ResetToken>,
}

impl Seal {
    pub(super) fn new(config: &UdpClientConfig) -> Self {
        Self {
            key: config.server_key,
            policy: config.rekey,
            ..Self::default()
        }
    }

    /// The handshake finished: the two record halves and the session's
    /// reset token.
    pub(super) fn install(&mut self, sealer: Sealer, opener: Opener, token: ResetToken) {
        self.sealer = Some(SendHalf::new(sealer, self.policy, Instant::now()));
        self.opener = Some(opener);
        self.token = Some(token);
    }

    /// The control frame `seq` just went out for the first time (its
    /// counter confirms the key phase once ACKed).
    pub(super) fn sent_rel(&mut self, seq: u32) {
        if let Some(s) = self.sealer.as_mut() {
            s.sent_rel(seq);
        }
    }

    /// The server's cumulative ACK, for the key-phase confirmation.
    pub(super) fn on_ack(&mut self, ack: u32) {
        if let Some(s) = self.sealer.as_mut() {
            s.on_ack(ack);
        }
    }

    /// The send half (tests).
    #[cfg(test)]
    pub(super) fn send_half(&mut self) -> Option<&mut SendHalf> {
        self.sealer.as_mut()
    }
}

impl UdpClient {
    /// Whether this client seals (it pinned a server key).
    pub fn sealed(&self) -> bool {
        self.seal.key.is_some()
    }

    /// A client → server datagram as it goes on the wire: one SEALED
    /// record on a sealed session (the CID rides its header); on a
    /// plaintext one tagged with the CID once there is one (module
    /// `migrate`), untouched otherwise. `None` once the record counter is
    /// exhausted (2^62): the session is over (the band declared dead).
    pub(super) fn wire(&mut self, d: Vec<u8>) -> Option<Vec<u8>> {
        if let Some(sealer) = self.seal.sealer.as_mut() {
            let mut out = Vec::with_capacity(d.len() + crate::seal::wire::OVERHEAD_C2S);
            let sealed = sealer.seal(&d, &mut out, Instant::now());
            (self.stats.rekeys, self.stats.rekeys_unconfirmed) =
                (sealer.rekeys, sealer.unconfirmed);
            if sealed.is_err() {
                self.stats.seal_exhausted += 1;
                self.declare_rel_dead();
                return None;
            }
            return Some(out);
        }
        Some(match self.path.cid {
            Some(cid) => tag(cid, &d),
            None => d,
        })
    }

    /// One datagram from the server on a sealed session: its plaintext,
    /// or `None` — dropped, counted (an unsealed datagram, or a record
    /// refused under its name; past the integrity limit the session is
    /// over).
    pub(super) fn open_record(&mut self, d: &[u8]) -> Option<Vec<u8>> {
        let Some(opener) = self.seal.opener.as_mut() else {
            self.stats.unsealed_dropped += 1;
            return None;
        };
        let kind = d.first().copied().unwrap_or(0);
        if kind & !crate::seal::wire::KIND_PHASE_BIT != crate::seal::wire::KIND_SEALED {
            self.stats.unsealed_dropped += 1;
            return None;
        }
        match opener.open(d) {
            Ok(o) => Some(o.plaintext),
            Err(r) if r != Refusal::IntegrityLimit && self.stateless_reset(d) => None,
            Err(r) => {
                let s = &mut self.stats;
                *match r {
                    Refusal::IntegrityLimit => &mut s.seal_integrity_limit,
                    Refusal::Malformed => &mut s.seal_malformed,
                    Refusal::TooOld => &mut s.seal_too_old,
                    Refusal::Replayed => &mut s.seal_replayed,
                    Refusal::WrongPhase => &mut s.seal_wrong_phase,
                    Refusal::Forged => &mut s.seal_forged,
                } += 1;
                if r == Refusal::IntegrityLimit {
                    self.declare_rel_dead();
                }
                None
            }
        }
    }

    /// A refused datagram that may be a stateless reset: `true` — the
    /// session is over, counted `stateless_resets_received` (and nothing
    /// else) — when it has a reset's shape and its last 16 bytes are the
    /// session's token, compared in constant time. A reset-sized one
    /// whose tail is not the token is counted `stateless_resets_invalid`
    /// (beside its refusal's name: a reset from a server whose reset key
    /// changed and a forged small record look alike, by design).
    fn stateless_reset(&mut self, d: &[u8]) -> bool {
        let (Some(tail), Some(token)) = (reset_tail(d), self.seal.token.as_ref()) else {
            return false;
        };
        if !token.matches(tail) {
            self.stats.stateless_resets_invalid += 1;
            return false;
        }
        self.stats.stateless_resets_received += 1;
        // The server no longer holds the session: what is outstanding is
        // never delivered (`gave_up`), and the caller sees the end.
        self.declare_rel_dead();
        true
    }
}
