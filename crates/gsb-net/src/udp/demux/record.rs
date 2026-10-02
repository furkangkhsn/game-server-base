//! The record layer, demux side (B5a, module `crate::udp::sealed`): on a
//! sealed door every session datagram is a SEALED record. It is routed by
//! the CID in its header, opened by the session's `Opener` (the replay
//! window and the key phases are there), and only the plaintext of a
//! record that opened goes on to the ordinary dispatch. Every refusal is
//! counted under its `seal_*` name. A child of [`super`], so the demux's
//! state stays private.
//!
//! **Migration (RUDP-SECURITY §7).** A record from an address that is not
//! its session's starts a path validation only when it opened
//! (authenticated — condition 1) AND it is the newest record the session
//! ever opened (condition 2); the challenge itself is sealed (condition
//! 3's proof can come only from the key holder). An older record from a
//! new address is processed — it is the client's — but moves nothing
//! (`udp_path_candidates_not_newest`). A sniffed CID is worth nothing: a
//! forged record from anywhere fails the AEAD before any of this.

use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use gsb_core::conn::ConnIn;
use gsb_protocol::{FrameBody, op};
use tracing::warn;

use super::SessionKey;
use crate::seal::Refusal;
use crate::seal::wire::{HEADER_LEN_C2S, TAG_LEN};
use crate::udp::sealed::encode_send;
use crate::udp::*;

impl super::Demux {
    /// One SEALED record of `n` bytes from `from` (a sealed door only).
    pub(super) fn handle_sealed(&mut self, n: usize, from: SocketAddr) {
        let cid = match n >= HEADER_LEN_C2S + TAG_LEN {
            true => u64_at(&self.buf[..n], 1),
            false => None,
        };
        let Some(cid) = cid else {
            self.count_refusal(Refusal::Malformed);
            return;
        };
        let Some(key) = self.sessions.key_of_cid(cid) else {
            self.mig.cid_unknown += 1;
            return;
        };
        let now = Instant::now();
        self.path_expiry(key, now);
        let Some(s) = self.sessions.get_mut(key) else {
            return;
        };
        let current = s.addr == from;
        if !current && !self.migration {
            // Migration off: an address change is a new session (as on
            // the plaintext door) — nothing of it is read.
            self.no_session += 1;
            return;
        }
        let Some(seal) = s.seal.as_mut() else {
            self.bad_datagrams += 1; // no plaintext session on a sealed door
            return;
        };
        let opened = match seal.opener.open(&self.buf[..n]) {
            Ok(o) => {
                // The client holds the session: no proof is re-answered.
                seal.accept = None;
                o
            }
            Err(r) => {
                self.count_refusal(r);
                if r == Refusal::IntegrityLimit {
                    self.end_at_limit(key);
                }
                return;
            }
        };
        let inner = opened.plaintext;
        let Some(&kind) = inner.first() else {
            self.bad_datagrams += 1;
            return;
        };
        match kind {
            KIND_PATH_RESPONSE => match u64_at(&inner, 1) {
                Some(nonce) => self.path_response(key, from, nonce),
                None => self.bad_datagrams += 1,
            },
            // Refused by rule, as on the plaintext door.
            KIND_FRAG => self.frag_refused += 1,
            KIND_RAW | KIND_REL | KIND_ACK | KIND_REPORT => {
                if !current {
                    match opened.newest {
                        true => self.path_candidate(key, from, n, now),
                        false => self.seal_counts().candidates_not_newest += 1,
                    }
                }
                // The plaintext is the datagram the plaintext door would
                // have read: the ordinary dispatch takes it from the buffer.
                self.buf[..inner.len()].copy_from_slice(&inner);
                self.dispatch(kind, 1, inner.len(), Some(key));
            }
            // A HELLO, a server-only kind or an unknown one: nothing a
            // client seals.
            _ => self.bad_datagrams += 1,
        }
    }

    /// Count one refused record under its name.
    fn count_refusal(&mut self, r: Refusal) {
        self.seal_counts().refusal(r);
    }

    /// The sealed door's counters (a sealed door only).
    pub(super) fn seal_counts(&mut self) -> &mut crate::udp::sealed::Counts {
        &mut self
            .seal
            .as_mut()
            .expect("a sealed-door path on a sealed door")
            .counts
    }

    /// The session hit the integrity limit: nothing of it opens again, so
    /// it ends — its actor told (best-effort), the session removed,
    /// counted.
    fn end_at_limit(&mut self, key: SessionKey) {
        let Some(s) = self.remove_session(key) else {
            return;
        };
        self.seal_counts().sessions_ended_limit += 1;
        let reason = "rUDP record layer: the integrity limit (2^36 forged records) was reached";
        warn!(peer = %s.addr, %reason, "rUDP: ending the session");
        let notice = ConnIn::ServerClosed {
            cause: gsb_core::conn::ServerClose::StreamRejected,
            reason: reason.into(),
        };
        if s.in_tx.try_send(notice).is_err() {
            self.removed_actor_gone += 1;
        }
    }

    /// Ask a sealed session's writer to seal and send `inner` (to `to`, or
    /// to the session's address): `false` when its channel refused.
    pub(super) fn queue_send(&self, key: SessionKey, to: Option<SocketAddr>, inner: &[u8]) -> bool {
        let Some(s) = self.sessions.get(key) else {
            return false;
        };
        let fb = FrameBody::new(op::base::UDP_SEND, Bytes::from(encode_send(to, inner)));
        s.out_tx.try_send(vec![fb]).is_ok()
    }
}
