//! The session's game-band estimate (what the congestion response,
//! module `crate::udp::congestion`, reads) and the windowed minimum round
//! trip it measures queueing delay against (BACKLOG B93). A CHILD of
//! [`super`].

use std::time::{Duration, Instant};

/// How far back the windowed minimum RTT looks: between half of this and
/// all of it (two half-window buckets). Long enough that a queue the
/// session's own pacing drains shows the path's floor at least once,
/// short enough that a route that got longer stops reading as a standing
/// queue within seconds.
pub(in crate::udp) const RTT_WINDOW: Duration = Duration::from_secs(10);

/// The session's game-band estimate. The smoothed RTT is the reliable
/// band's (`rel::Rto`), which the probes feed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct GameEstimate {
    /// The newest probe round trip, and the smallest one seen.
    pub(crate) latest_rtt: Duration,
    pub(crate) min_rtt: Duration,
    /// The smallest round trip of the last [`RTT_WINDOW`] (B93): the
    /// path's floor as it is now — the lifetime minimum would make a
    /// route that got longer look like a standing queue forever.
    pub(crate) window_min_rtt: Duration,
    /// The last interval between two answered probes: its length, the
    /// game datagrams sent in it, their bytes, and how many of them the
    /// client missed.
    pub(crate) interval: Duration,
    pub(crate) interval_sent: u64,
    pub(crate) interval_sent_bytes: u64,
    pub(crate) interval_lost: u64,
    /// The loss fraction, smoothed over the intervals that sent anything
    /// (weight 1/4 per interval; the first such interval sets it).
    pub(crate) loss: f64,
    /// The intervals that sent anything.
    pub(crate) loss_intervals: u64,
    /// Answered probes so far.
    pub(crate) reports: u64,
}

/// The windowed minimum: two half-window buckets, the current one and
/// the one before. Constant state, no per-sample history.
#[derive(Debug, Clone, Copy)]
pub(super) struct WindowMin {
    cur: Option<Duration>,
    prev: Option<Duration>,
    started: Instant,
}

impl WindowMin {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            cur: None,
            prev: None,
            started: now,
        }
    }

    /// Take a sample; the minimum over the window, this sample included.
    pub(super) fn sample(&mut self, rtt: Duration, now: Instant) -> Duration {
        let half = RTT_WINDOW / 2;
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= RTT_WINDOW {
            // Silent for a whole window: nothing in it is current.
            (self.prev, self.cur, self.started) = (None, None, now);
        } else if elapsed >= half {
            (self.prev, self.cur, self.started) = (self.cur, None, now);
        }
        let cur = self.cur.map_or(rtt, |c| c.min(rtt));
        self.cur = Some(cur);
        self.prev.map_or(cur, |p| p.min(cur))
    }
}
