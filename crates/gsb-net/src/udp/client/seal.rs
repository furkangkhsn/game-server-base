//! The record layer, client side (B5a, module `crate::udp::sealed`): a
//! client that pinned the server's public key
//! ([`UdpClientConfig::server_key`]) runs Noise NK inside the cookie
//! handshake and, once the server's message 2 authenticated, seals every
//! datagram it sends and opens every one it receives. A datagram that
//! does not open is dropped and counted under its `seal_*` name in
//! [`UdpClientStats`]. A child of [`super`], so it reaches the client's
//! private state directly.

use super::*;
use crate::seal::{Opener, Refusal, Sealer};

/// The client's record state.
#[derive(Default)]
pub(super) struct Seal {
    /// The pinned server key (`None`: a plaintext client).
    pub(super) key: Option<[u8; crate::seal::KEY_LEN]>,
    sealer: Option<Sealer>,
    opener: Option<Opener>,
}

impl Seal {
    pub(super) fn new(key: Option<[u8; crate::seal::KEY_LEN]>) -> Self {
        Self {
            key,
            ..Self::default()
        }
    }

    /// The handshake finished: the two record halves.
    pub(super) fn install(&mut self, sealer: Sealer, opener: Opener) {
        self.sealer = Some(sealer);
        self.opener = Some(opener);
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
            if sealer.seal(&d, &mut out).is_err() {
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
}
