//! The delay signal's inputs (rUDP hardening round 4, BACKLOG B104): the
//! session's newest round trips and the path's jitter, from which the
//! controller reads a queue that jitter cannot fake and the threshold it
//! must stand over. Pure: every sample carries its own clock. A CHILD of
//! [`super`].
//!
//! - **The queue is a minimum.** A single round trip says nothing on a
//!   jittery path: one unlucky sample stands over the windowed floor
//!   (the luckiest sample of seconds) by the jitter alone. The smallest
//!   of the newest few samples does not move with jitter — some of them
//!   are lucky too — but it moves with a standing queue, which delays
//!   every one of them ([`RttTrack::recent_min`]; LEDBAT's "current
//!   delay" filter, RFC 6817 §3.4.2, against BBR's windowed floor).
//! - **The threshold is the path's jitter, at least the limit.** A jitter
//!   of σ puts the window's floor some 2σ under the mean, so a sample
//!   over floor + [`JITTER_MULT`]·σ is some 2σ over the mean — one in
//!   forty draws on a jittery path, four in a row one in millions, with
//!   room left for the estimate's own error while it is young; on a path
//!   without jitter the threshold is the old [`QUEUE_DELAY_LIMIT`], and
//!   no path's jitter hides a queue of [`QUEUE_DELAY_MAX`].
//! - **Jitter is what a straight line cannot follow.** A queue grows and
//!   drains smoothly between samples a quarter-second apart; jitter does
//!   not. The estimate is the mean deviation of a sample from the line
//!   through its two neighbours (time-weighted: the cadence changes), so
//!   a queue that fills or drains at a steady pace adds nothing to it.
//!   Only samples taken at the fast cadence raise it (a suspected or
//!   paced session; [`RttTrack::sample`]'s `fast`): at one a second, a
//!   queue that other sessions' pacing raises and lowers every few
//!   seconds looks like jitter, and a session that took it for jitter
//!   would never yield its share (measured: one latecomer in a shared
//!   bottleneck kept the link). At the slow cadence a deviation can only
//!   lower it — one spike learned while suspected does not blind an
//!   open session for good. Each fast deviation counts at most
//!   [`JITTER_CLIP`] times the estimate (at least the limit): the corner
//!   of a cut or of a queue's onset, where the line breaks once, does
//!   not inflate it. The first [`JITTER_SAMPLES`] fast deviations are
//!   averaged, then it moves by 1/[`JITTER_SAMPLES`] (RFC 3550's
//!   interarrival jitter gain).

use std::time::{Duration, Instant};

use super::{QUEUE_DELAY_LIMIT, QUEUE_DELAY_MAX, QUEUE_SAMPLES};

/// The threshold over the path's jitter.
pub(in crate::udp) const JITTER_MULT: f64 = 4.0;
/// A deviation counts at most this many times the jitter estimate (at
/// least [`QUEUE_DELAY_LIMIT`]).
pub(in crate::udp) const JITTER_CLIP: f64 = 3.0;
/// Deviations averaged before the estimate moves by a fixed gain of
/// 1/this.
pub(in crate::udp) const JITTER_SAMPLES: u32 = 16;

/// One round trip and when its report arrived.
#[derive(Debug, Clone, Copy)]
struct Sample {
    rtt: Duration,
    at: Instant,
}

/// See the module docs.
#[derive(Debug, Default)]
pub(in crate::udp) struct RttTrack {
    /// The newest samples, oldest first.
    recent: [Option<Sample>; QUEUE_SAMPLES],
    /// The jitter estimate (seconds) and the deviations it averaged.
    jitter: f64,
    deviations: u32,
    /// Whether the newest sample was taken at the fast cadence.
    fast: bool,
}

impl RttTrack {
    /// One more round trip, arrived `at`; `fast` when the session was
    /// probed at the fast cadence (suspected or paced).
    pub(in crate::udp) fn sample(&mut self, rtt: Duration, at: Instant, fast: bool) {
        let [.., Some(a), Some(b)] = self.recent else {
            return self.push(rtt, at, fast);
        };
        if fast == self.fast {
            let span = at.saturating_duration_since(a.at).as_secs_f64();
            let part = b.at.saturating_duration_since(a.at).as_secs_f64();
            let (ra, rb, rc) = (a.rtt.as_secs_f64(), b.rtt.as_secs_f64(), rtt.as_secs_f64());
            let line = if span > 0.0 {
                ra + (rc - ra) * part / span
            } else {
                (ra + rc) / 2.0
            };
            let d = (rb - line).abs();
            if fast {
                self.deviate(d);
            }
        }
        self.push(rtt, at, fast);
    }

    /// A fast-cadence deviation: averaged, then a fixed gain; clipped.
    fn deviate(&mut self, d: f64) {
        let clip = (JITTER_CLIP * self.jitter).max(QUEUE_DELAY_LIMIT.as_secs_f64());
        self.deviations = self.deviations.saturating_add(1);
        let gain = 1.0 / f64::from(self.deviations.min(JITTER_SAMPLES));
        self.jitter += (d.min(clip) - self.jitter) * gain;
    }

    fn push(&mut self, rtt: Duration, at: Instant, fast: bool) {
        self.recent.rotate_left(1);
        self.recent[QUEUE_SAMPLES - 1] = Some(Sample { rtt, at });
        self.fast = fast;
    }

    /// The smallest of the newest `n` samples (at most
    /// [`QUEUE_SAMPLES`]); `None` until there are `n`.
    pub(in crate::udp) fn recent_min(&self, n: usize) -> Option<Duration> {
        let newest = &self.recent[QUEUE_SAMPLES - n.min(QUEUE_SAMPLES)..];
        newest
            .iter()
            .map(|s| s.map(|s| s.rtt))
            .try_fold(Duration::MAX, |m, r| Some(m.min(r?)))
    }

    /// The smallest sample there is (`None`: none yet).
    pub(in crate::udp) fn floor(&self) -> Option<Duration> {
        self.recent.iter().flatten().map(|s| s.rtt).min()
    }

    /// The fast-cadence deviations the jitter estimate rests on.
    #[cfg(test)]
    pub(in crate::udp) fn deviations(&self) -> u32 {
        self.deviations
    }

    /// The queueing delay that is a signal on this path.
    pub(in crate::udp) fn threshold(&self) -> Duration {
        Duration::from_secs_f64(self.jitter * JITTER_MULT).clamp(QUEUE_DELAY_LIMIT, QUEUE_DELAY_MAX)
    }
}
