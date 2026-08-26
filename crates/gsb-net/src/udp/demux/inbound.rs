//! The demux's inbound data path: forwarding decoded frames to a
//! session, the reliable band's ordering/ACK bookkeeping, and the
//! datagram-kind dispatch that guards every parse.

use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use gsb_core::conn::ConnIn;
use gsb_protocol::op;
use gsb_protocol::FrameBody;
use tracing::{debug, warn};

use crate::udp::*;

impl super::Demux {
    /// Forward one decoded frame into the session's mailbox; return
    /// `true` if the session must be removed (its actor is gone). The
    /// caller must then call `remove_session`.
    fn forward(&mut self, peer: SocketAddr, fb: FrameBody) -> bool {
        let Some(s) = self.sessions.get_mut(&peer) else {
            return false;
        };
        // The peer is alive: reset its idle window (push the new entry;
        // the old one becomes stale and is swept lazily). Copy out of the
        // borrow first — the deadline insert touches a different field.
        let now = Instant::now();
        let idle = self.idle;
        s.last_seen = now;
        if let Some(idle) = idle {
            self.deadlines.insert((now + idle, peer));
        }
        match s.in_tx.try_send(ConnIn::Frame(fb)) {
            Ok(()) => false,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // Backpressure without a per-connection socket: a full
                // mailbox means this session's actor is stuck
                // (downstream — room/registry — is not consuming). Drop
                // the frame, stay isolated (never stall the demux, i.e.
                // every other session), count it.
                s.inbox_full += 1;
                if !s.inbox_full_warned {
                    s.inbox_full_warned = true;
                    warn!(
                        %peer,
                        "rUDP: session mailbox full; inbound frames for this \
                         session are being dropped (isolated: other sessions \
                         are unaffected)"
                    );
                }
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                // The actor exited (budget close, shutdown, …): the
                // caller removes the session (the idle sweep would find
                // it too, but this is immediate).
                self.removed_actor_gone += 1;
                true
            }
        }
    }

    /// Inbound reliable (client→server): dedupe + order + forward + ACK.
    ///
    /// Structured as two phases: PHASE 1 decides, under the session
    /// borrow, what to forward (in sequence order) and whether to ACK;
    /// PHASE 2 performs the forwarding/ACK via `self` methods (which
    /// must not run while the session borrow is live).
    fn handle_rel(&mut self, peer: SocketAddr, seq: u32, body: Vec<u8>) {
        // PHASE 1.
        let (to_forward, ack_to) = {
            let Some(s) = self.sessions.get_mut(&peer) else {
                return; // no session: drop (pre-handshake or already gone)
            };
            // The peer is alive: reset its idle window.
            let now = Instant::now();
            let idle = self.idle;
            s.last_seen = now;
            if let Some(idle) = idle {
                self.deadlines.insert((now + idle, peer));
            }
            let mut to_forward: Vec<FrameBody> = Vec::new();
            let mut ack_to: Option<u32> = None;
            match seq.cmp(&s.in_expected) {
                std::cmp::Ordering::Equal => {
                    if let Some(fb) = body_of(&body, 0) {
                        to_forward.push(fb);
                    }
                    s.in_expected = s.in_expected.wrapping_add(1);
                    // Flush the contiguous tail of the out-of-order
                    // window (control frames are delivered in sequence
                    // order, never out of it).
                    while let Some(gap) = s.in_oob.remove(&s.in_expected) {
                        if let Some(fb) = body_of(&gap, 0) {
                            to_forward.push(fb);
                        }
                        s.in_expected = s.in_expected.wrapping_add(1);
                    }
                    ack_to = Some(s.in_expected);
                }
                std::cmp::Ordering::Less => {
                    // Duplicate (its ACK was presumably lost): re-ACK
                    // only — never re-forward (correctness: control
                    // frames run exactly once, in order).
                    s.dup_in += 1;
                    ack_to = Some(s.in_expected);
                }
                std::cmp::Ordering::Greater => {
                    // Gap: buffer (bounded) and do NOT advance the
                    // cumulative ACK — the client retransmits the
                    // missing frame.
                    if s.in_oob.len() < OOB_CAP {
                        s.in_oob.insert(seq, body);
                    } else {
                        s.oob_dropped += 1;
                    }
                }
            }
            (to_forward, ack_to)
        };
        // PHASE 2 (the borrow above is over).
        for fb in to_forward {
            if self.forward(peer, fb) {
                self.remove_session(peer);
                return;
            }
        }
        if let Some(next) = ack_to {
            self.send_ack(peer, next);
        }
    }

    fn send_ack(&mut self, peer: SocketAddr, next: u32) {
        let ack = encode_ack(next);
        if let Err(e) = self.sock.try_send_to(&ack, peer) {
            debug!(%peer, %e, "rUDP: ack send failed (best-effort)");
        }
    }

    pub(super) fn handle(&mut self, n: usize, peer: SocketAddr) {
        if n < 1 || n > self.max_datagram {
            self.oversized_in += 1;
            return;
        }
        match self.buf[0] {
            KIND_HELLO => {
                if n < 18 {
                    self.bad_datagrams += 1;
                    return;
                }
                self.handle_hello(peer);
            }
            KIND_ACK => {
                if n < 5 {
                    self.bad_datagrams += 1;
                    return;
                }
                let ack = u32::from_le_bytes(self.buf[1..5].try_into().unwrap());
                // Piggyback the ACK into the session's OUTBOUND channel:
                // the writer (the only reader of it) applies it to its
                // retransmit state. This is the one transport-internal
                // round trip: it needs no command channel (the demux
                // cannot await one — its only awaited source is the
                // socket) and costs one bounded-channel send.
                if let Some(s) = self.sessions.get(&peer) {
                    let fb =
                        FrameBody::new(op::base::UDP_ACK, Bytes::from(ack.to_le_bytes().to_vec()));
                    match s.out_tx.try_send(vec![fb]) {
                        Ok(()) => self.acks_piggybacked += 1,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            // The writer is stuck; the retransmit will
                            // run to RETRANSIT_MAX and give up (bounded).
                            self.ack_piggyback_failed += 1;
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            self.removed_actor_gone += 1;
                            self.remove_session(peer);
                        }
                    }
                }
            }
            KIND_REL => {
                if n < 7 {
                    self.bad_datagrams += 1;
                    return;
                }
                let seq = u32::from_le_bytes(self.buf[1..5].try_into().unwrap());
                let body = self.buf[5..n].to_vec();
                self.handle_rel(peer, seq, body);
            }
            KIND_RAW => {
                if let Some(fb) = body_of(&self.buf[1..n], 0) {
                    // RAW (lossy game band): no seq, no order, no ACK.
                    if self.forward(peer, fb) {
                        self.remove_session(peer);
                    }
                } else {
                    self.bad_datagrams += 1;
                }
            }
            _ => self.bad_datagrams += 1,
        }
    }
}
