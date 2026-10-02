//! The game band's congestion response (rUDP hardening round 3, BACKLOG
//! B1): what a session's writer does when its client's reports say the
//! path carries less than the room sends.
//!
//! **The transport measures and paces; thinning content is the game's.**
//! A session whose path rate is below the room's demand does not queue
//! without bound (that is latency): its writer paces the game band to
//! the estimated rate and DROPS THE OLDEST game-band frames it cannot
//! send within [`QUEUE_BUDGET`], each counted
//! (`udp_game_frames_dropped_paced`). The control band is never paced
//! or dropped — its bytes are charged to the same budget, so the game
//! band yields to it. What the room should send instead is the game's
//! decision; the transport says what the path carries ([`PathState`]).
//!
//! **Opt-in** ([`UdpCongestion`]; the server's `udp_congestion`): `Off`
//! is the writer as it was. `Pace` changes nothing for a client that
//! never reports (no estimate, so never anything but [`PathPhase::Open`])
//! and nothing while a reporting client's path keeps up: an open session
//! sends at once, as it always did.
//!
//! **The controller** ([`Control`], pure — every input is an argument):
//!
//! - *Signals — both, per probe interval.* Loss: the interval lost at
//!   least [`LOSS_MIN`] game datagrams and at least 1/[`LOSS_DIV`] of
//!   them (a policer or a shallow buffer: loss without delay). Delay: the
//!   newest probe round trip is [`QUEUE_DELAY_LIMIT`] over the windowed
//!   minimum (`feedback::RTT_WINDOW`, B93) — a deep buffer filling, the
//!   latency a game feels long before the buffer overflows.
//! - *Burst vs sustained.* An open session that sees one signal is only
//!   *suspected* ([`PathPhase::Suspect`]: probed every
//!   [`FAST_PROBE_INTERVAL`], still unpaced); a second signal in a row
//!   paces it, a clean interval clears it. One bad interval (a burst)
//!   costs nothing but a few faster probes.
//! - *The rate.* Entering, and on every signal while paced: the rate
//!   the path DELIVERED in the interval, times [`BETA`] (never above the
//!   rate already paced to). Delivered bytes are the interval's sent
//!   bytes times its delivered fraction (the report counts datagrams);
//!   the span is the interval as the client saw it — the send-time
//!   interval stretched by the growth of the round trip (a filling queue
//!   spreads the same datagrams over a longer receive span), so a
//!   growing queue does not read as capacity. A delay signal while the
//!   round trip is already falling is the last cut draining the queue,
//!   not a new signal: the paced rate holds (loss still cuts) — without
//!   this hold a deep buffer is cut into every 250 ms while it drains
//!   (measured: a quarter more cuts, less delivered). Each clean interval adds
//!   [`INCREASE`] datagram budgets per second per second (additive, so
//!   sessions sharing a bottleneck converge to equal shares); a rate at
//!   [`EXIT_HEADROOM`] × the room's demand opens the session again. A
//!   whole ring of probes unanswered while paced halves it (no sample:
//!   the classic timeout response). Floor: [`MIN_RATE`] datagram budgets
//!   per second.

use std::time::{Duration, Instant};

use crate::udp::feedback::{GameEstimate, PROBE_INTERVAL};

mod queue;
pub(super) use queue::PaceQueue;
mod state;
pub use state::{PathPhase, PathState, UdpCongestion};

#[cfg(test)]
mod tests;

/// The probe interval of a suspected or paced session.
pub(super) const FAST_PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// Queueing delay over the windowed minimum RTT that is a signal.
pub(super) const QUEUE_DELAY_LIMIT: Duration = Duration::from_millis(30);
/// A loss signal: at least this many datagrams lost in the interval…
pub(super) const LOSS_MIN: u64 = 2;
/// …and at least 1/this of the interval's datagrams.
pub(super) const LOSS_DIV: u64 = 10;
/// The decrease factor on a signal.
pub(super) const BETA: f64 = 0.85;
/// The additive increase per clean interval, in datagram budgets per
/// second per second of interval.
pub(super) const INCREASE: f64 = 16.0;
/// The floor, in datagram budgets per second.
pub(super) const MIN_RATE: f64 = 4.0;
/// A paced session opens again once its rate is this much over the
/// room's demand.
pub(super) const EXIT_HEADROOM: f64 = 1.25;
/// The longest a paced game-band frame may wait for the path: the
/// pacing queue holds at most rate × this many bytes (and always the
/// newest frame); older frames are dropped and counted.
pub(super) const QUEUE_BUDGET: Duration = Duration::from_millis(50);
/// The pacer's burst: its token bucket holds rate × this (and always at
/// least one datagram).
pub(super) const PACE_BURST: Duration = Duration::from_millis(10);

/// What the controller did, for the session's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ControlCounts {
    /// Times the session entered the paced phase.
    pub(super) episodes: u64,
    /// Rate decreases (the entry's included).
    pub(super) cuts: u64,
}

/// One session's congestion controller (see the module docs).
#[derive(Debug)]
pub(super) struct Control {
    phase: PathPhase,
    /// Bytes per second (meaningful while paced).
    rate: f64,
    min_rate: f64,
    increase: f64,
    /// The previous report's round trip (the receive-span correction).
    prev_rtt: Option<Duration>,
    /// Game-band bytes the room offered since the last report, and the
    /// demand they made (bytes per second, the last report interval).
    offered: u64,
    since: Instant,
    demand: f64,
    queue_delay: Duration,
    loss: f64,
    pub(super) counts: ControlCounts,
}

impl Control {
    pub(super) fn new(max_datagram: usize, now: Instant) -> Self {
        let budget = max_datagram as f64;
        Self {
            phase: PathPhase::Open,
            rate: 0.0,
            min_rate: MIN_RATE * budget,
            increase: INCREASE * budget,
            prev_rtt: None,
            offered: 0,
            since: now,
            demand: 0.0,
            queue_delay: Duration::ZERO,
            loss: 0.0,
            counts: ControlCounts::default(),
        }
    }

    /// The session moved to a new path (module `crate::udp::path`): the
    /// controller starts over, open and unpaced (RFC 9000 §9.4 resets the
    /// congestion controller); its counters stay.
    pub(super) fn new_path(&mut self, now: Instant) {
        self.phase = PathPhase::Open;
        self.rate = 0.0;
        self.prev_rtt = None;
        (self.offered, self.since, self.demand) = (0, now, 0.0);
        self.queue_delay = Duration::ZERO;
        self.loss = 0.0;
    }

    /// The room handed the game band `bytes` more (sent or queued).
    pub(super) fn offered(&mut self, bytes: usize) {
        self.offered += bytes as u64;
    }

    /// The pacing rate in bytes per second, while paced.
    pub(super) fn paced(&self) -> Option<f64> {
        (self.phase == PathPhase::Paced).then_some(self.rate)
    }

    /// The probe interval the session needs now.
    pub(super) fn probe_interval(&self) -> Duration {
        match self.phase {
            PathPhase::Open => PROBE_INTERVAL,
            PathPhase::Suspect | PathPhase::Paced => FAST_PROBE_INTERVAL,
        }
    }

    /// A report was applied: `e` is the session's estimate after it.
    pub(super) fn on_estimate(&mut self, e: &GameEstimate, now: Instant) {
        let elapsed = now.saturating_duration_since(self.since).as_secs_f64();
        if elapsed > 0.0 {
            self.demand = self.offered as f64 / elapsed;
        }
        (self.offered, self.since) = (0, now);
        let (loss, delay) = (loss_signal(e), delay_signal(e));
        // A standing queue that is already shrinking is the last cut
        // working, not a new signal: a paced session holds its rate.
        let draining = self.prev_rtt.is_some_and(|p| e.latest_rtt < p);
        let signal = loss || delay;
        let delivered = delivered_rate(e, self.prev_rtt);
        self.prev_rtt = Some(e.latest_rtt);
        self.queue_delay = e.latest_rtt.saturating_sub(e.window_min_rtt);
        self.loss = e.loss;
        match (self.phase, signal) {
            (PathPhase::Open, false) => {}
            (PathPhase::Open, true) => self.phase = PathPhase::Suspect,
            (PathPhase::Suspect, false) => self.phase = PathPhase::Open,
            (PathPhase::Suspect, true) => {
                self.phase = PathPhase::Paced;
                self.counts.episodes += 1;
                self.cut(delivered.unwrap_or(self.demand), f64::INFINITY);
            }
            (PathPhase::Paced, true) if !loss && draining => {}
            (PathPhase::Paced, true) => self.cut(delivered.unwrap_or(self.rate), self.rate),
            (PathPhase::Paced, false) => {
                self.rate += self.increase * e.interval.as_secs_f64();
                if self.rate >= self.demand * EXIT_HEADROOM {
                    self.phase = PathPhase::Open;
                }
            }
        }
    }

    /// A whole ring of probes went unanswered: no sample at all. A paced
    /// session halves its rate; an open one is only silent (B91).
    pub(super) fn on_silence(&mut self) {
        if self.phase == PathPhase::Paced {
            self.rate = (self.rate / 2.0).max(self.min_rate);
            self.counts.cuts += 1;
        }
    }

    fn cut(&mut self, delivered: f64, ceiling: f64) {
        self.rate = (delivered.min(ceiling) * BETA).max(self.min_rate);
        self.counts.cuts += 1;
    }

    /// The session's path as the game reads it (`gsb_core::path`): the
    /// phase and, while paced, the rate; once a report was applied on
    /// this path, the measurements too — the room's demand, the smoothed
    /// loss, the newest round trip and the queue over the windowed floor.
    /// Before that (no report yet, or a new path) only the phase: the
    /// default state.
    pub(super) fn state(&self) -> PathState {
        let measured = self.prev_rtt.is_some();
        PathState {
            phase: self.phase,
            rate: self.paced().map(|r| r.min(f64::from(u32::MAX)) as u32),
            demand: measured.then(|| self.demand.min(f64::from(u32::MAX)) as u32),
            loss_permille: measured.then(|| (self.loss * 1000.0).round().clamp(0.0, 1000.0) as u16),
            rtt: self.prev_rtt,
            queue_delay: measured.then_some(self.queue_delay),
        }
    }
}

/// The interval lost enough to be congestion, not noise.
fn loss_signal(e: &GameEstimate) -> bool {
    e.interval_lost >= LOSS_MIN && e.interval_lost * LOSS_DIV >= e.interval_sent
}

/// The newest round trip stands a queue over the path's floor.
fn delay_signal(e: &GameEstimate) -> bool {
    e.latest_rtt >= e.window_min_rtt + QUEUE_DELAY_LIMIT
}

/// The bytes per second the path delivered in the interval (see the
/// module docs): `None` when it sent nothing.
fn delivered_rate(e: &GameEstimate, prev_rtt: Option<Duration>) -> Option<f64> {
    if e.interval_sent == 0 || e.interval.is_zero() {
        return None;
    }
    let got = (e.interval_sent - e.interval_lost.min(e.interval_sent)) as f64;
    let bytes = e.interval_sent_bytes as f64 * got / e.interval_sent as f64;
    let interval = e.interval.as_secs_f64();
    let stretch = prev_rtt.map_or(0.0, |p| e.latest_rtt.as_secs_f64() - p.as_secs_f64());
    Some(bytes / (interval + stretch).max(interval / 2.0))
}
