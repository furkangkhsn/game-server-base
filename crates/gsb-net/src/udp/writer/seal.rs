//! The record layer, writer side (B5a, module `crate::udp::sealed`): on
//! a sealed door the writer owns the session's server → client
//! `Sealer`, so EVERY datagram to the client — game, control, a re-sent
//! control frame (a fresh record counter each time), a probe, and the
//! demux's own ACKs and path challenges (`UDP_SEND`) — is sealed here,
//! one counter space with one owner. A CHILD of [`super`], so the
//! writer's state stays private.

use std::borrow::Cow;
use std::time::Instant;

use gsb_core::conn::ConnIn;
use gsb_protocol::FrameBody;
use tracing::warn;

use crate::seal::wire::OVERHEAD_S2C;
use crate::udp::sealed::decode_send;
use crate::udp::*;

impl super::UdpWriter {
    /// `inner` as it goes on the wire: one SEALED record on a sealed door
    /// (the header is the AEAD's associated data), the bytes themselves on
    /// a plaintext one. `None` once the record counter is exhausted (2^62
    /// — the session ends on the loop's next turn, counted).
    pub(super) fn wire<'a>(&mut self, inner: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        let Some(sealer) = self.sealer.as_mut() else {
            return Some(Cow::Borrowed(inner));
        };
        let mut out = Vec::with_capacity(inner.len() + OVERHEAD_S2C);
        match sealer.seal(inner, &mut out, Instant::now()) {
            Ok(_) => Some(Cow::Owned(out)),
            Err(_) => {
                self.seal_exhausted = true;
                None
            }
        }
    }

    /// The demux's `UDP_SEND` (a sealed door's ACK or path challenge):
    /// seal it and send it where the demux said — the session's address,
    /// or a candidate path's. Synchronous like the retransmit pass; a
    /// refusal is counted by what the datagram carried, and the peer's
    /// own re-send (or the next challenge round) retries.
    pub(super) fn apply_send(&mut self, frame: &FrameBody) {
        let Some((to, inner)) = decode_send(&frame.payload) else {
            return; // the demux sends only whole requests
        };
        let challenge = inner[0] == KIND_PATH_CHALLENGE;
        let Some(d) = self.wire(inner).map(Cow::into_owned) else {
            return;
        };
        match self.sock.try_send_to(&d, to.unwrap_or(self.peer)) {
            Ok(_) => self.pace_charge(d.len()),
            Err(_) if challenge => self.sends_challenge_failed += 1,
            Err(_) => self.sends_ack_failed += 1,
        }
    }

    /// The control frame `seq` just went out for the first time: on a
    /// sealed session its record counter is kept for the ACK that covers
    /// it (the peer's confirmation of the key phase — module
    /// `crate::udp::sealed`, `rekey`).
    pub(super) fn sent_rel(&mut self, seq: u32) {
        if let Some(s) = self.sealer.as_mut() {
            s.sent_rel(seq);
        }
    }

    /// The record counter ran out: no datagram of this session can be
    /// sealed again, so it ends — loudly, counted, with the stream
    /// rejected (the transport refuses to carry it further).
    pub(super) fn die_sealed(&mut self) {
        self.ended_seal_limit += 1;
        let reason = "rUDP record layer: the session's record counter is exhausted".to_string();
        warn!(conn = %self.conn, peer = %self.peer, %reason, "rUDP: ending the session");
        self.post_verdict(ConnIn::ServerClosed {
            cause: gsb_core::conn::ServerClose::StreamRejected,
            reason,
        });
    }

    /// Whether this writer seals (tests).
    #[cfg(test)]
    pub(in crate::udp) fn sealed(&self) -> bool {
        self.sealer.is_some()
    }
}
