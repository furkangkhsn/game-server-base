//! The demux's datagram-kind dispatch: every parse is guarded here, and
//! every datagram is routed to its session — by the source address
//! (untagged, as always) or by the connection id (a tagged datagram,
//! module `crate::udp::path`). A child of [`super`], so the demux's state
//! stays private.

use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use gsb_protocol::FrameBody;
use gsb_protocol::op;

use super::SessionKey;
use crate::udp::path::{RESPONSE_LEN, TAGGED_HEADER};
use crate::udp::*;

impl super::Demux {
    pub(super) fn handle(&mut self, n: usize, from: SocketAddr) {
        if n < 1 || n > self.max_datagram {
            self.oversized_in += 1;
            return;
        }
        let kind = self.buf[0];
        if kind == KIND_HELLO {
            if n < 18 {
                self.bad_datagrams += 1;
                return;
            }
            self.handle_hello(n, from);
            return;
        }
        if self.seal.is_some() {
            // A sealed door (B5a, module `record`): after the handshake
            // every datagram is a SEALED record; anything else is dropped
            // unread, counted.
            match kind & !crate::seal::wire::KIND_PHASE_BIT == crate::seal::wire::KIND_SEALED {
                true => self.handle_sealed(n, from),
                false => self.seal_counts().datagrams_unsealed += 1,
            }
            return;
        }
        if kind & KIND_CID_TAG != 0 && self.migration {
            self.handle_tagged(n, from);
            return;
        }
        // Untagged (or a tag this server does not grant: an unknown kind,
        // as it always was): by the source address.
        let key = self.sessions.key_at(&from);
        if let Some(key) = key {
            self.path_expiry(key, Instant::now());
        }
        self.dispatch(kind, 1, n, key);
    }

    /// A tagged datagram (`[k | 0x80][u64 cid][body]`): the session is
    /// the CID's, wherever it comes from. From an address that is not the
    /// session's, it is a path candidate (module `crate::udp::path`) —
    /// and still the session's datagram.
    fn handle_tagged(&mut self, n: usize, from: SocketAddr) {
        let Some(cid) = u64_at(&self.buf[..n], 1) else {
            self.bad_datagrams += 1;
            return;
        };
        let Some(key) = self.sessions.key_of_cid(cid) else {
            self.mig.cid_unknown += 1;
            return;
        };
        let now = Instant::now();
        self.path_expiry(key, now);
        let inner = self.buf[0] & !KIND_CID_TAG;
        if inner == KIND_PATH_RESPONSE {
            if n < RESPONSE_LEN {
                self.bad_datagrams += 1;
                return;
            }
            let nonce = u64_at(&self.buf[..n], TAGGED_HEADER).unwrap_or(0);
            self.path_response(key, from, nonce);
            return;
        }
        if inner == KIND_FRAG {
            // Refused by rule, as untagged (no reassembly here); not a
            // datagram a client sends, so no path candidate either.
            self.frag_refused += 1;
            return;
        }
        if !matches!(inner, KIND_RAW | KIND_REL | KIND_ACK | KIND_REPORT) {
            // A HELLO, a server-only kind or an unknown one, tagged:
            // nothing a client sends — malformed, the session untouched.
            self.bad_datagrams += 1;
            return;
        }
        let current = self.sessions.get(key).is_some_and(|s| s.addr == from);
        if !current {
            self.path_candidate(key, from, n, now);
        }
        self.dispatch(inner, TAGGED_HEADER, n, Some(key));
    }

    /// One datagram of kind `kind` whose body is `buf[at..n]`, for the
    /// session `key` (`None`: no session at its source address).
    pub(super) fn dispatch(&mut self, kind: u8, at: usize, n: usize, key: Option<SessionKey>) {
        let body = n - at;
        match kind {
            KIND_ACK => {
                if body < 4 {
                    self.bad_datagrams += 1;
                    return;
                }
                let Some(key) = key else {
                    // An ACK from an address with no session (B66).
                    self.no_session += 1;
                    return;
                };
                let ack = u32::from_le_bytes(self.buf[at..at + 4].try_into().unwrap());
                self.forward_ack(key, ack);
            }
            KIND_REL => {
                if body < 6 {
                    self.bad_datagrams += 1;
                    return;
                }
                let Some(key) = key else {
                    // No session: drop (pre-handshake or already gone),
                    // counted (B66).
                    self.no_session += 1;
                    return;
                };
                let seq = u32::from_le_bytes(self.buf[at..at + 4].try_into().unwrap());
                let rest = self.buf[at + 4..n].to_vec();
                self.handle_rel(key, seq, rest);
            }
            KIND_RAW => {
                let Some(fb) = body_of(&self.buf[at..n], 0) else {
                    self.bad_datagrams += 1;
                    return;
                };
                // RAW (lossy game band): no seq, no order, no ACK.
                match key {
                    None => self.no_session += 1, // B66
                    Some(key) => {
                        if self.forward(key, fb) {
                            self.remove_session(key);
                        }
                    }
                }
            }
            KIND_REPORT => {
                if body < 8 {
                    self.bad_datagrams += 1;
                    return;
                }
                self.handle_report(key, at);
            }
            KIND_FRAG => {
                // Refused: the server never reassembles (inputs are small,
                // and reassembly state here would be memory any session
                // could make the server hold — `docs/SECURITY.md` §4.1).
                // Nothing is forwarded; the session is not otherwise
                // touched (not even its idle window).
                self.frag_refused += 1;
            }
            _ => self.bad_datagrams += 1,
        }
    }

    /// Piggyback an ACK into the session's OUTBOUND channel: the writer
    /// (the only reader of it) applies it to its retransmit state. This
    /// is the one transport-internal round trip: it needs no command
    /// channel (the demux cannot await one — its only awaited source is
    /// the socket) and costs one bounded-channel send.
    fn forward_ack(&mut self, key: SessionKey, ack: u32) {
        let Some(s) = self.sessions.get(key) else {
            return;
        };
        let fb = FrameBody::new(op::base::UDP_ACK, Bytes::from(ack.to_le_bytes().to_vec()));
        match s.out_tx.try_send(vec![fb]) {
            Ok(()) => self.acks_piggybacked += 1,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // The writer is stuck; the peer's retransmit re-asks, and
                // the REL liveness bound ends a band that stops
                // progressing (bounded).
                self.ack_piggyback_failed += 1;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.removed_actor_gone += 1;
                self.remove_session(key);
            }
        }
    }
}
