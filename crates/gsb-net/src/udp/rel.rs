//! The reliable control band's SENDING half, shared by the server's
//! per-session writer and the client: the un-ACKed queue, the cumulative
//! ACK, the liveness clock ("The REL liveness bound") and the retransmit
//! timer ([`Rto`], BACKLOG B2). Pure state: every method takes `now`,
//! so each rule is tested without a socket or a sleep; the owner does
//! the I/O and the counting.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::udp::REL_NO_ACK_FATAL;

mod rto;
pub(super) use rto::{HANDSHAKE_MAX_RTO, MAX_RTO, Rto};
#[cfg(test)]
pub(super) use rto::{INITIAL_RTO, MIN_RTO};

/// One un-ACKed control frame.
#[derive(Debug)]
pub(super) struct Outstanding {
    pub(super) seq: u32,
    pub(super) datagram: Bytes,
    /// When it was last put on the wire.
    pub(super) sent: Instant,
    /// Whether it was ever sent again: Karn's rule takes no RTT sample
    /// from its ACK.
    pub(super) resent: bool,
}

/// What the retransmit pass must do now.
#[derive(Debug, PartialEq)]
pub(super) enum Due {
    /// Nothing outstanding.
    Idle,
    /// Something outstanding, its timer still running.
    Wait,
    /// The oldest frame's timer expired: send these bytes again, then
    /// call [`RelSend::resent`] (only if the socket took them).
    Resend(Bytes),
    /// No cumulative-ACK progress for this long while something was
    /// outstanding: the band is dead.
    Dead(Duration),
}

/// The sending half of one direction of the reliable band.
#[derive(Debug)]
pub(super) struct RelSend {
    /// Un-ACKed frames in seq order; the owner bounds it (`RETRANSIT_CAP`).
    queue: VecDeque<Outstanding>,
    /// Highest cumulative ACK received (the next seq the peer expects).
    acked: u32,
    /// When the cumulative ACK last advanced — or, while nothing is
    /// outstanding, simply "now" (an idle band has nothing to prove).
    ack_progress: Instant,
    /// When the queue last became empty (an ACK released its last
    /// frame): the start of an idle spell.
    emptied: Instant,
    rto: Rto,
}

impl RelSend {
    /// A fresh band (the peer expects seq 1) with a timer `rto`.
    pub(super) fn new(now: Instant, rto: Rto) -> Self {
        Self {
            queue: VecDeque::new(),
            acked: 1,
            ack_progress: now,
            emptied: now,
            rto,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.queue.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// The retransmit timer (read-only: the band drives it).
    pub(super) fn rto(&self) -> &Rto {
        &self.rto
    }

    /// The oldest outstanding frame (tests rewind its clock).
    #[cfg(test)]
    pub(super) fn front_mut(&mut self) -> Option<&mut Outstanding> {
        self.queue.front_mut()
    }

    /// Move the liveness clock `by` into the past (tests stand in for a
    /// long silence without sleeping).
    #[cfg(test)]
    pub(super) fn rewind_progress(&mut self, by: Duration) {
        self.ack_progress -= by;
    }

    /// A control frame was just sent for the first time. The liveness
    /// clock measures unanswered WORK, so it starts when something
    /// becomes outstanding: a frame after a quiet spell must not inherit
    /// a clock stamped at the last ACK. After an idle spell longer than
    /// the timer, the timer also drops its backoff
    /// ([`Rto::restart_after_idle`]).
    pub(super) fn push(&mut self, seq: u32, datagram: Bytes, now: Instant) {
        if self.queue.is_empty() {
            self.ack_progress = now;
            if now.saturating_duration_since(self.emptied) >= self.rto.current() {
                self.rto.restart_after_idle();
            }
        }
        self.queue.push_back(Outstanding {
            seq,
            datagram,
            sent: now,
            resent: false,
        });
    }

    /// Apply a cumulative ACK (`ack` = the next seq the peer expects):
    /// release every frame below it and — only when it actually moved —
    /// restart the liveness clock. The RTT sample is the NEWEST released
    /// frame's (the ACK was sent when it arrived), and there is none when
    /// any released frame was ever re-sent (Karn's rule: the ACK may
    /// answer either copy, and a gap filled by a re-sent frame delays
    /// the ACK of every frame behind it).
    pub(super) fn on_ack(&mut self, ack: u32, now: Instant) {
        if ack > self.acked {
            self.acked = ack;
            // Real progress: the channel is demonstrably alive.
            self.ack_progress = now;
        }
        let mut newest = None;
        let mut resent = false;
        while let Some(front) = self.queue.front() {
            if front.seq >= self.acked {
                break;
            }
            resent |= front.resent;
            newest = Some(front.sent);
            self.queue.pop_front();
            if self.queue.is_empty() {
                self.emptied = now;
            }
        }
        if let (Some(sent), false) = (newest, resent) {
            self.rto.sample(now.saturating_duration_since(sent));
        }
    }

    /// The retransmit pass's question (see [`Due`]). An individual frame
    /// is NEVER abandoned: the band as a whole dies at
    /// [`REL_NO_ACK_FATAL`] without ACK progress.
    pub(super) fn poll(&mut self, now: Instant) -> Due {
        let Some(front) = self.queue.front() else {
            self.ack_progress = now;
            return Due::Idle;
        };
        let stalled = now.saturating_duration_since(self.ack_progress);
        if stalled >= REL_NO_ACK_FATAL {
            return Due::Dead(stalled);
        }
        if front.sent + self.rto.current() <= now {
            Due::Resend(front.datagram.clone())
        } else {
            Due::Wait
        }
    }

    /// The oldest frame went out again: its timer restarts, doubled.
    pub(super) fn resent(&mut self, now: Instant) {
        if let Some(front) = self.queue.front_mut() {
            front.sent = now;
            front.resent = true;
            self.rto.timed_out();
        }
    }

    /// How long the owner may wait before its next pass: until the oldest
    /// frame's timer expires, but never longer than `tick` (the owner's
    /// housekeeping interval) — and a whole `tick` when the timer has
    /// already expired (the socket refused the re-send; retrying at once
    /// would spin).
    pub(super) fn wait(&self, now: Instant, tick: Duration) -> Duration {
        let Some(front) = self.queue.front() else {
            return tick;
        };
        match (front.sent + self.rto.current()).checked_duration_since(now) {
            Some(left) if !left.is_zero() => left.min(tick),
            _ => tick,
        }
    }

    /// The band died: forget what is outstanding and return how many.
    pub(super) fn abandon(&mut self) -> u64 {
        let n = self.queue.len() as u64;
        self.queue.clear();
        n
    }
}

#[cfg(test)]
mod tests;
