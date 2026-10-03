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
//!   of σ puts the window's floor some 2σ under the mean, so a queue
//!   over floor + [`JITTER_MULT`]·σ is some 3σ over the mean — a sample
//!   a jittery path draws about once in 700, two in a row almost never,
//!   with room left for the estimate's own error; on a path without
//!   jitter the threshold is [`QUEUE_DELAY_LIMIT`], as in round 3, and no
//!   path's jitter hides a queue of [`QUEUE_DELAY_MAX`] (the B104
//!   scenarios: σ up to 40 ms a direction, some 55 ms a round trip —
//!   the threshold 275 ms; a jitter-free path keeps the round-3 30 ms).
//! - **Jitter is what a parabola cannot follow.** A queue moves smoothly
//!   between samples a quarter-second apart — it fills or drains at a
//!   steady pace, or (a paced session's own additive increase) at a
//!   steadily growing one; jitter does not. The estimate is the mean
//!   distance of the newest sample from the parabola through the three
//!   before it (Lagrange, so an uneven cadence is no trend either),
//!   scaled by the noise that extrapolation carries so that it is σ for
//!   normal jitter: a queue that fills, drains or accelerates adds
//!   nothing to it (a straight line was not enough: the queue a paced
//!   session's own growth builds bends, and a line read it as jitter).
//!   Only samples taken at the fast cadence count (a suspected or paced
//!   session; [`RttTrack::sample`]'s `fast`): at one a second, a queue
//!   that other sessions' pacing raises and lowers every few seconds
//!   looks like jitter, and a session that took it for jitter would
//!   never yield its share. Each distance counts at most [`JITTER_CLIP`]
//!   times the estimate — at least the jitter whose threshold is
//!   [`QUEUE_DELAY_LIMIT`] (so the corner of a cut, where no parabola
//!   fits, does not inflate a calm path's estimate), and while the
//!   estimate is young (its first [`JITTER_SAMPLES`] distances) at least
//!   the one whose threshold is [`QUEUE_DELAY_MAX`] (so a jittery path's
//!   estimate grows to its jitter in a few samples). The first
//!   [`JITTER_SAMPLES`] distances are averaged, then it moves by
//!   1/[`JITTER_SAMPLES`] (RFC 3550's interarrival jitter gain).

use std::time::{Duration, Instant};

use super::{QUEUE_DELAY_LIMIT, QUEUE_DELAY_MAX};

/// The threshold over the path's jitter.
pub(in crate::udp) const JITTER_MULT: f64 = 5.0;
/// A distance counts at most this many times the jitter estimate (at
/// least the jitter whose threshold is [`QUEUE_DELAY_LIMIT`]; while the
/// estimate is young, [`QUEUE_DELAY_MAX`]).
pub(in crate::udp) const JITTER_CLIP: f64 = 3.0;
/// Distances averaged before the estimate moves by a fixed gain of
/// 1/this.
pub(in crate::udp) const JITTER_SAMPLES: u32 = 16;
/// The round trips kept: the newest and the three its parabola needs.
const KEPT: usize = 4;

/// One round trip, when its report arrived, and whether it was probed
/// at the fast cadence.
#[derive(Debug, Clone, Copy)]
struct Sample {
    rtt: Duration,
    at: Instant,
    fast: bool,
}

/// See the module docs.
#[derive(Debug, Default)]
pub(in crate::udp) struct RttTrack {
    /// The newest samples, oldest first.
    recent: [Option<Sample>; KEPT],
    /// The jitter estimate (seconds) and the distances it averaged.
    jitter: f64,
    deviations: u32,
}

impl RttTrack {
    /// One more round trip, arrived `at`; `fast` when the session was
    /// probed at the fast cadence (suspected or paced).
    pub(in crate::udp) fn sample(&mut self, rtt: Duration, at: Instant, fast: bool) {
        let new = Sample { rtt, at, fast };
        if let [.., Some(a), Some(b), Some(c)] = self.recent
            && fast
            && b.fast
            && c.fast
            && let Some(d) = distance([a, b, c], new)
        {
            self.deviate(d);
        }
        self.recent.rotate_left(1);
        self.recent[KEPT - 1] = Some(new);
    }

    /// One fast-cadence distance: averaged, then a fixed gain; clipped.
    fn deviate(&mut self, d: f64) {
        let young = self.deviations < JITTER_SAMPLES;
        let least = if young {
            QUEUE_DELAY_MAX
        } else {
            QUEUE_DELAY_LIMIT
        };
        let clip = (JITTER_CLIP * self.jitter).max(least.as_secs_f64() / JITTER_MULT);
        self.deviations = self.deviations.saturating_add(1);
        let gain = 1.0 / f64::from(self.deviations.min(JITTER_SAMPLES));
        self.jitter += (d.min(clip) - self.jitter) * gain;
    }

    /// The smallest of the newest `n` samples (at most four); `None`
    /// until there are `n`.
    pub(in crate::udp) fn recent_min(&self, n: usize) -> Option<Duration> {
        let newest = &self.recent[KEPT - n.min(KEPT)..];
        newest
            .iter()
            .map(|s| s.map(|s| s.rtt))
            .try_fold(Duration::MAX, |m, r| Some(m.min(r?)))
    }

    /// The smallest sample there is (`None`: none yet).
    pub(in crate::udp) fn floor(&self) -> Option<Duration> {
        self.recent.iter().flatten().map(|s| s.rtt).min()
    }

    /// The fast-cadence distances the jitter estimate rests on (the
    /// controller paces on a queue under [`QUEUE_DELAY_MAX`] only once
    /// there are enough).
    pub(in crate::udp) fn deviations(&self) -> u32 {
        self.deviations
    }

    /// The queueing delay that is a signal on this path.
    pub(in crate::udp) fn threshold(&self) -> Duration {
        Duration::from_secs_f64(self.jitter * JITTER_MULT).clamp(QUEUE_DELAY_LIMIT, QUEUE_DELAY_MAX)
    }
}

/// How far `new` lies from the parabola through `p` (seconds), divided
/// by the spread jitter alone gives that distance, times √(π/2): for
/// independent normal jitter of σ its mean is σ, whatever the spacing.
/// `None` when two samples share an instant (no parabola).
fn distance(p: [Sample; 3], new: Sample) -> Option<f64> {
    let t = |s: &Sample| s.at.saturating_duration_since(p[0].at).as_secs_f64();
    let (x, x1, x2) = (t(&new), t(&p[1]), t(&p[2]));
    // Lagrange's weights of the three samples at the new one's instant.
    let l = [
        (x - x1) * (x - x2) / (x1 * x2),
        x * (x - x2) / (x1 * (x1 - x2)),
        x * (x - x1) / (x2 * (x2 - x1)),
    ];
    if l.iter().any(|w| !w.is_finite()) {
        return None;
    }
    let fit: f64 = l.iter().zip(&p).map(|(w, s)| w * s.rtt.as_secs_f64()).sum();
    let spread = (1.0 + l.iter().map(|w| w * w).sum::<f64>()).sqrt();
    let half_pi = std::f64::consts::FRAC_PI_2;
    Some((new.rtt.as_secs_f64() - fit).abs() / spread * half_pi.sqrt())
}
