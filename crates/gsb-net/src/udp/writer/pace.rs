//! The writer's congestion response (module `crate::udp::congestion`):
//! the controller fed by the session's reports, the pacing queue game
//! frames wait in while the session is paced, and the pass that releases
//! them. A CHILD of [`super`], so the writer's state stays private.
//!
//! Nothing here runs with the response off, and nothing is queued while
//! the session is open: the game band is sent at once, as it always was.

use std::time::{Duration, Instant};

use tracing::debug;

use crate::udp::congestion::{Control, PaceQueue, PathState, QUEUE_BUDGET, UdpCongestion};

/// The writer's congestion state.
#[derive(Debug)]
pub(super) struct Pace {
    on: bool,
    pub(super) control: Control,
    pub(super) queue: PaceQueue,
}

impl Pace {
    pub(super) fn new(mode: UdpCongestion, max_datagram: usize, now: Instant) -> Self {
        Self {
            on: mode == UdpCongestion::Pace,
            control: Control::new(max_datagram, now),
            queue: PaceQueue::new(max_datagram, now),
        }
    }

    /// The pacing queue's byte cap at `rate`.
    fn cap(rate: f64) -> usize {
        (rate * QUEUE_BUDGET.as_secs_f64()) as usize
    }
}

impl super::UdpWriter {
    /// Hand one game-band message (its datagrams: one RAW, or a FRAG
    /// set) to the pacer. `Some` gives it back to be sent at once: the
    /// response is off, or the session is open with nothing queued.
    pub(super) fn pace_offer(
        &mut self,
        datagrams: Vec<Vec<u8>>,
        frag: bool,
    ) -> Option<Vec<Vec<u8>>> {
        if !self.pace.on {
            return Some(datagrams);
        }
        let p = &mut self.pace;
        p.control.offered(datagrams.iter().map(Vec::len).sum());
        let cap = match p.control.paced() {
            Some(rate) => Pace::cap(rate),
            None if p.queue.is_empty() => return Some(datagrams),
            // Opened again with frames still queued: behind them, in
            // order; the pass releases them all at once.
            None => usize::MAX,
        };
        p.queue.push(datagrams, frag, cap);
        None
    }

    /// Release what the pacer affords now: paced, what the bucket holds;
    /// open again, everything still queued.
    pub(super) async fn pace_pass(&mut self) {
        if !self.pace.on {
            return;
        }
        loop {
            let released = match self.pace.control.paced() {
                Some(rate) => self.pace.queue.pop(Instant::now(), rate),
                None => self.pace.queue.pop_any(),
            };
            let Some(r) = released else { break };
            self.send(&r.datagram, false).await;
            if r.frag {
                self.frag_datagrams += 1;
                self.frag_messages += u64::from(r.last);
            }
        }
    }

    /// How long the writer may wait for its next batch: the reliable
    /// band's retransmit timer (at most the housekeeping tick) and, while
    /// paced, the pacer's next release — one awaited source, the pacing
    /// deadline joins the existing `min`.
    pub(super) fn wake(&self, now: Instant) -> Duration {
        let wait = self.rel.wait(now, crate::udp::RETRANSIT_TICK);
        match self.pace.control.paced() {
            Some(rate) => self
                .pace
                .queue
                .wait(now, rate)
                .map_or(wait, |w| w.min(wait)),
            None => wait,
        }
    }

    /// A control datagram went out: the game band yields its bytes.
    pub(super) fn pace_charge(&mut self, bytes: usize) {
        if let Some(rate) = self.pace.control.paced() {
            self.pace.queue.charge(Instant::now(), rate, bytes);
        }
    }

    /// A report was applied: the controller decides, the queue follows
    /// the new rate, the probes the new cadence.
    pub(super) fn pace_report(&mut self, now: Instant) {
        let Some(e) = self.feedback.estimate().filter(|_| self.pace.on) else {
            return;
        };
        let before = self.pace.control.state();
        self.pace.control.on_estimate(&e, now);
        self.pace_follow(before, now);
    }

    /// A whole ring of probes went unanswered (`Feedback::probe_sent`).
    pub(super) fn pace_silence(&mut self, now: Instant) {
        if self.pace.on {
            let before = self.pace.control.state();
            self.pace.control.on_silence();
            self.pace_follow(before, now);
        }
    }

    fn pace_follow(&mut self, before: PathState, now: Instant) {
        let after = self.pace.control.state();
        if let Some(rate) = self.pace.control.paced() {
            if before.rate.is_none() {
                self.pace.queue.start(now, rate);
            }
            self.pace.queue.trim(Pace::cap(rate));
        }
        self.feedback
            .set_interval(self.pace.control.probe_interval());
        if after.phase != before.phase || after.rate != before.rate {
            debug!(conn = %self.conn, peer = %self.peer, ?after, "rUDP: path state");
        }
    }

    /// The session is over for the transport: what the pacer still holds
    /// is never sent (counted).
    pub(super) fn pace_abandon(&mut self) {
        self.pace.queue.abandon();
    }

    /// The session's path as the game would read it (the follow-up
    /// carries it to the room — DESIGN §6 "Tıkanıklık tepkisi").
    pub(super) fn path_state(&self) -> PathState {
        self.pace.control.state()
    }
}
