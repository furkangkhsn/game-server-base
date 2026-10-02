//! The demux's inbound data path: forwarding decoded frames to a
//! session, and the reliable band's ordering/ACK bookkeeping. The
//! datagram-kind dispatch that guards every parse is `dispatch`.

use std::net::SocketAddr;
use std::time::Instant;

use gsb_core::conn::ConnIn;
use gsb_protocol::FrameBody;
use tracing::{debug, warn};

use super::SessionKey;
use crate::udp::*;

impl super::Demux {
    /// Forward one decoded frame into the session's mailbox; return
    /// `true` if the session must be removed (its actor is gone). The
    /// caller must then call `remove_session`.
    pub(super) fn forward(&mut self, key: SessionKey, fb: FrameBody) -> bool {
        // The peer is alive: reset its idle window (push the new entry;
        // the old one becomes stale and is swept lazily).
        self.heard(key, Instant::now());
        let Some(s) = self.sessions.get_mut(key) else {
            return false;
        };
        let peer = s.addr;
        let kind = gsb_core::conn::FrameKind::of(fb.op);
        match s.in_tx.try_send(ConnIn::Frame(fb)) {
            Ok(()) => false,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // Backpressure without a per-connection socket: a full
                // mailbox means this session's actor is stuck
                // (downstream — room/registry — is not consuming). Drop
                // the frame, stay isolated (never stall the demux, i.e.
                // every other session), count it.
                s.inbox_full += 1;
                // By kind (B58): a request dropped here was already
                // acknowledged on the reliable band, so the client never
                // re-sends it — a term of the RPC ledger.
                let n = match kind {
                    gsb_core::conn::FrameKind::Request => &mut self.full_requests,
                    gsb_core::conn::FrameKind::Action => &mut self.full_actions,
                    gsb_core::conn::FrameKind::Control => &mut self.full_controls,
                };
                *n += 1;
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
                // it too, but this is immediate). The frame is lost —
                // counted by kind (B66), like the `Full` arm.
                self.removed_actor_gone += 1;
                self.count_closed(kind);
                true
            }
        }
    }

    /// One decoded frame lost to a closed session inbox, by kind (B66): a
    /// request is a term of the RPC ledger.
    fn count_closed(&mut self, kind: gsb_core::conn::FrameKind) {
        let n = match kind {
            gsb_core::conn::FrameKind::Request => &mut self.closed_requests,
            gsb_core::conn::FrameKind::Action => &mut self.closed_actions,
            gsb_core::conn::FrameKind::Control => &mut self.closed_controls,
        };
        *n += 1;
    }

    /// Inbound reliable (client→server): dedupe + order + forward + ACK.
    ///
    /// Structured as two phases: PHASE 1 decides, under the session
    /// borrow, what to forward (in sequence order) and whether to ACK;
    /// PHASE 2 performs the forwarding/ACK via `self` methods (which
    /// must not run while the session borrow is live).
    pub(super) fn handle_rel(&mut self, key: SessionKey, seq: u32, body: Vec<u8>) {
        // The peer is alive: reset its idle window.
        self.heard(key, Instant::now());
        // PHASE 1.
        let (to_forward, ack_to, peer) = {
            let Some(s) = self.sessions.get_mut(key) else {
                return;
            };
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
            (to_forward, ack_to, s.addr)
        };
        // PHASE 2 (the borrow above is over).
        let mut frames = to_forward.into_iter();
        while let Some(fb) = frames.next() {
            if self.forward(key, fb) {
                // The frames behind it in sequence order are lost with the
                // session (B66); none of them was acknowledged.
                for rest in frames {
                    self.count_closed(gsb_core::conn::FrameKind::of(rest.op));
                }
                self.remove_session(key);
                return;
            }
        }
        if let Some(next) = ack_to {
            self.send_ack(key, peer, next);
        }
    }

    /// The reliable band's cumulative ACK to `key`'s client at `peer`. On
    /// a sealed door the session's writer seals and sends it (module
    /// `record`: one owner of the record counter); a refusal is counted
    /// and the client's re-send asks again.
    pub(super) fn send_ack(&mut self, key: SessionKey, peer: SocketAddr, next: u32) {
        let ack = encode_ack(next);
        if self.seal.is_some() {
            if !self.queue_send(key, None, &ack) {
                self.seal_counts().acks_not_queued += 1;
            }
            return;
        }
        if let Err(e) = self.sock.try_send_to(&ack, peer) {
            self.acks_send_failed += 1;
            debug!(%peer, %e, "rUDP: ack send failed (best-effort)");
        }
    }
}
