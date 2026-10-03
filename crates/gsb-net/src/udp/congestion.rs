//! The game band's congestion response (rUDP hardening round 3, BACKLOG
//! B1; its delay signal, increase and floor reworked by round 4, B104):
//! what a session's writer does when its client's reports say the path
//! carries less than the room sends.
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
//!   them (a policer or a shallow buffer: loss without delay). Delay: a
//!   standing queue — the smallest of the newest round trips stands over
//!   the windowed minimum (`feedback::RTT_WINDOW`, B93) by the path's
//!   threshold: [`QUEUE_DELAY_LIMIT`] on a steady path, five times its
//!   jitter on a jittery one, never over [`QUEUE_DELAY_MAX`] (module
//!   `signal`). A deep buffer filling, the latency a game feels long
//!   before the buffer overflows — and not jitter, which the round-3
//!   test (the newest sample [`QUEUE_DELAY_LIMIT`] over the floor) took
//!   for a queue: under netem jitter it paced healthy sessions and cut
//!   their snapshots by up to 34 % (B104).
//! - *Burst vs sustained.* An open session whose newest round trip is
//!   [`QUEUE_DELAY_LIMIT`] over the floor, or whose interval lost enough,
//!   is only *suspected* ([`PathPhase::Suspect`]: probed every
//!   [`FAST_PROBE_INTERVAL`], still unpaced) — cheap, and the fast
//!   samples are what the jitter estimate learns from. Two loss signals
//!   in a row pace it; so does a queue standing over the threshold in
//!   the newest [`QUEUE_SAMPLES`] round trips, once the jitter estimate
//!   rests on [`JITTER_KNOWN`] samples (a young estimate cannot tell a
//!   jittery path from a queue; a queue over [`QUEUE_DELAY_MAX`] needs
//!   no estimate). [`SUSPECT_REPORTS`] reports without either open it
//!   again; a queue that stands but is not yet known keeps it suspected.
//! - *The rate.* Entering: the rate the path DELIVERED in the interval,
//!   times [`BETA`]. While paced, each report decides once: a loss signal
//!   cuts (to the delivered rate times [`BETA`], never above the rate
//!   already paced to); a draining queue — the newest round trip fell by
//!   more than [`DRAIN_MIN`] since the last report — holds the rate (the
//!   last cut is working: another would empty the path, growing would
//!   refill the queue before it drained); a queue over the threshold in
//!   the newest [`PACED_QUEUE_SAMPLES`] cuts (a paced path is known to
//!   be congested: it reacts at once); a clean report adds [`INCREASE`]
//!   of the room's demand per second — the episode's highest demand, so
//!   a room that thins its content to the budget does not slow its own
//!   recovery, nor a noisy interval open the session early. Additive, so
//!   sessions sharing a bottleneck converge to equal shares; and a share
//!   of each session's demand, so a room's sessions behind one link grow
//!   together by a share of what they ask (round 3's fixed step, sized
//!   for four sessions, was most of the B104 link per interval for 64).
//!   A rate at [`EXIT_HEADROOM`] × that demand opens the session again.
//!   Delivered bytes are the interval's sent bytes times its delivered
//!   fraction (the report counts datagrams); the span is the interval as
//!   the client saw it — stretched by the growth of the queue (the
//!   smallest of the newest two round trips: one jittered sample does not
//!   move it), so a growing queue does not read as capacity. A whole ring
//!   of probes unanswered while paced halves the rate (no sample: the
//!   classic timeout response). Floor: [`MIN_RATE`] datagram budget per
//!   second — the newest frame still reaches the client, and a room's
//!   sessions behind one link can go below it together (round 4: at four
//!   budgets each, the B104 run's 64 sessions' floor was its 3.6 Mbit/s
//!   link itself).

use std::time::{Duration, Instant};

use crate::udp::feedback::{GameEstimate, PROBE_INTERVAL};

mod queue;
pub(super) use queue::PaceQueue;
mod signal;
use signal::RttTrack;
mod state;
pub use state::{PathPhase, PathState, UdpCongestion};

#[cfg(test)]
mod tests;

/// The probe interval of a suspected or paced session.
pub(super) const FAST_PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// Queueing delay over the windowed minimum RTT that suspects a session,
/// and the threshold of a path without jitter.
pub(super) const QUEUE_DELAY_LIMIT: Duration = Duration::from_millis(30);
/// The threshold's ceiling: a standing queue this long is a signal on
/// any path, however jittery, and paces before the jitter is known.
pub(super) const QUEUE_DELAY_MAX: Duration = Duration::from_millis(300);
/// The newest round trips whose smallest must stand over the threshold
/// to pace a suspected session…
pub(super) const QUEUE_SAMPLES: usize = 2;
/// …and to cut a paced one.
pub(super) const PACED_QUEUE_SAMPLES: usize = 1;
/// The reports without a signal after which a suspected session opens.
pub(super) const SUSPECT_REPORTS: u32 = 4;
/// The fast-cadence samples the jitter estimate must rest on before a
/// standing queue (under [`QUEUE_DELAY_MAX`]) paces a session.
pub(super) const JITTER_KNOWN: u32 = 4;
/// A paced session's queue is draining when its newest round trip fell
/// by more than this since the last report.
pub(super) const DRAIN_MIN: Duration = Duration::from_millis(5);
/// A loss signal: at least this many datagrams lost in the interval…
pub(super) const LOSS_MIN: u64 = 2;
/// …and at least 1/this of the interval's datagrams.
pub(super) const LOSS_DIV: u64 = 10;
/// The decrease factor on a signal.
pub(super) const BETA: f64 = 0.85;
/// The additive increase per clean report: this share of the episode's
/// highest demand, per second of interval.
pub(super) const INCREASE: f64 = 0.25;
/// The floor, in datagram budgets per second.
pub(super) const MIN_RATE: f64 = 1.0;
/// A paced session opens again once its rate is this much over the
/// episode's highest demand.
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
    /// The previous report's round trip (the path state's).
    prev_rtt: Option<Duration>,
    /// The newest round trips and the path's jitter (the delay signal);
    /// the previous report's recent minimum (a draining queue) and queue
    /// level (the receive-span correction).
    track: RttTrack,
    prev_recent: Option<Duration>,
    prev_level: Option<Duration>,
    /// Reports since the session was suspected; whether the last report
    /// was a loss signal.
    suspect_reports: u32,
    last_loss: bool,
    /// Game-band bytes the room offered since the last report, and the
    /// demand they made (bytes per second, the last report interval);
    /// the episode's highest demand (the increase's base and the exit's).
    offered: u64,
    since: Instant,
    demand: f64,
    peak_demand: f64,
    queue_delay: Duration,
    loss: f64,
    pub(super) counts: ControlCounts,
}

impl Control {
    pub(super) fn new(max_datagram: usize, now: Instant) -> Self {
        Self {
            phase: PathPhase::Open,
            rate: 0.0,
            min_rate: MIN_RATE * max_datagram as f64,
            prev_rtt: None,
            track: RttTrack::default(),
            prev_recent: None,
            prev_level: None,
            suspect_reports: 0,
            last_loss: false,
            offered: 0,
            since: now,
            demand: 0.0,
            peak_demand: 0.0,
            queue_delay: Duration::ZERO,
            loss: 0.0,
            counts: ControlCounts::default(),
        }
    }

    /// The session moved to a new path (module `crate::udp::path`): the
    /// controller starts over, open and unpaced, its samples and jitter
    /// forgotten (RFC 9000 §9.4 resets the congestion controller); its
    /// counters and floor stay.
    pub(super) fn new_path(&mut self, now: Instant) {
        let (min_rate, counts) = (self.min_rate, self.counts);
        *self = Self::new(0, now);
        (self.min_rate, self.counts) = (min_rate, counts);
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
        let loss = loss_signal(e);
        let last_loss = std::mem::replace(&mut self.last_loss, loss);
        self.prev_rtt = Some(e.latest_rtt);
        self.loss = e.loss;
        // The delay signal (module `signal`): a sample of the jitter at
        // the fast cadence, the queue's level, and whether a queue stands
        // over the path's threshold in the newest samples.
        self.track
            .sample(e.latest_rtt, now, self.phase != PathPhase::Open);
        let floor = e.window_min_rtt;
        let level = self.track.recent_min(QUEUE_SAMPLES);
        let growth = level
            .zip(self.prev_level)
            .map_or(0.0, |(l, p)| l.as_secs_f64() - p.as_secs_f64());
        self.prev_level = level;
        let delivered = delivered_rate(e, growth);
        let over = floor + self.track.threshold();
        let samples = match self.phase {
            PathPhase::Paced => PACED_QUEUE_SAMPLES,
            PathPhase::Open | PathPhase::Suspect => QUEUE_SAMPLES,
        };
        let recent = self.track.recent_min(samples);
        let standing = recent.is_some_and(|r| r >= over);
        let draining = recent
            .zip(self.prev_recent)
            .is_some_and(|(r, p)| r + DRAIN_MIN < p);
        self.prev_recent = recent;
        let recent_floor = self.track.floor().unwrap_or(e.latest_rtt);
        self.queue_delay = recent_floor.saturating_sub(floor);
        match self.phase {
            PathPhase::Open => {
                if loss || e.latest_rtt >= floor + QUEUE_DELAY_LIMIT {
                    (self.phase, self.suspect_reports) = (PathPhase::Suspect, 0);
                }
            }
            PathPhase::Suspect => {
                self.suspect_reports += 1;
                let known = self.track.deviations() >= JITTER_KNOWN
                    || recent.is_some_and(|r| r >= floor + QUEUE_DELAY_MAX);
                if (loss && last_loss) || (standing && known) {
                    self.phase = PathPhase::Paced;
                    self.counts.episodes += 1;
                    self.peak_demand = self.demand;
                    self.cut(delivered.unwrap_or(self.demand), f64::INFINITY);
                } else if !standing && self.suspect_reports >= SUSPECT_REPORTS {
                    self.phase = PathPhase::Open;
                }
            }
            PathPhase::Paced => {
                self.peak_demand = self.peak_demand.max(self.demand);
                if loss || (standing && !draining) {
                    self.cut(delivered.unwrap_or(self.rate), self.rate);
                } else if !draining {
                    self.rate += INCREASE * self.peak_demand * e.interval.as_secs_f64();
                    if self.rate >= self.peak_demand * EXIT_HEADROOM {
                        self.phase = PathPhase::Open;
                    }
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
    /// loss, the newest round trip and the queue: the smallest of the
    /// newest round trips over the windowed floor (jitter does not raise
    /// it; module `signal`). Before that (no report yet, or a new path)
    /// only the phase: the default state.
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

/// The bytes per second the path delivered in the interval, its receive
/// span stretched by `growth` seconds of queue (see the module docs):
/// `None` when it sent nothing.
fn delivered_rate(e: &GameEstimate, growth: f64) -> Option<f64> {
    if e.interval_sent == 0 || e.interval.is_zero() {
        return None;
    }
    let got = (e.interval_sent - e.interval_lost.min(e.interval_sent)) as f64;
    let bytes = e.interval_sent_bytes as f64 * got / e.interval_sent as f64;
    let interval = e.interval.as_secs_f64();
    Some(bytes / (interval + growth).max(interval / 2.0))
}
