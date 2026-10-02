//! A QUIC session's path, read off quinn's own statistics (BACKLOG
//! B103, `gsb_core::path`): what this door tells the room about the
//! connection it carries.
//!
//! quinn already runs a congestion controller (its window, its pacer);
//! this does not second-guess it — it reads it. Every
//! [`SAMPLE_EVERY`] while the writer moves bytes ([`PathFeed::wrote`],
//! called by the send half — a session that sends nothing has no path
//! news), one `Connection::stats()` gives:
//!
//! | quinn 0.11 (`ConnectionStats`) | `PathState` |
//! |---|---|
//! | `path.rtt` (smoothed) | `rtt` |
//! | `path.rtt` over its own windowed minimum | `queue_delay` |
//! | Δ`path.lost_packets` / Δ`path.sent_packets` | `loss_permille` |
//! | Δ`udp_tx.bytes` / Δt | `demand` (the bytes the connection put on the wire) |
//! | `path.cwnd` / `path.rtt` | `rate` — only while `Paced` |
//! | Δ`path.congestion_events` (loss or ECN cut the window) | the phase |
//!
//! **The phase, as rUDP's** (`Open` → `Suspect` → `Paced`): an interval
//! with a congestion event makes an open path suspect and a suspect one
//! paced; a clean interval clears suspicion; a paced path opens again
//! once the window carries a quarter more than the connection offers
//! (`rate ≥ 1.25 × demand`). The rate is the congestion window's: what
//! quinn lets the connection have in flight per round trip. It is told
//! only while paced — an app-limited window does not grow, so a budget
//! read from an open path would hold the game below what the path could
//! take; open, the game sends as it always did.
//!
//! **Sent on change** (`gsb_core::path::PathSignal`): into the connection
//! actor's inbox with `try_send`, never awaited; a full inbox keeps the
//! newest state owed for the next sample, a closed one ends the feed.

mod feed;

#[cfg(test)]
mod tests;

pub(super) use feed::PathFeed;

use std::time::Duration;

use gsb_core::path::{PathPhase, PathState};
use tokio::time::Instant;

/// How often a sending session's statistics are read (rUDP's fast probe
/// cadence: a reaction within about a second, at four reads a second).
pub(super) const SAMPLE_EVERY: Duration = Duration::from_millis(250);

/// The window of the round-trip floor (two buckets: the minimum of the
/// last 5–10 s — a longer route is learnt, a queue is not taken for one).
const FLOOR_BUCKET: Duration = Duration::from_secs(5);

/// One read of quinn's statistics (the counters are cumulative).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Sample {
    pub(super) at: Instant,
    pub(super) rtt: Duration,
    pub(super) cwnd: u64,
    pub(super) congestion_events: u64,
    pub(super) lost_packets: u64,
    pub(super) sent_packets: u64,
    pub(super) tx_bytes: u64,
}

impl Sample {
    /// Read `conn`'s statistics now.
    pub(super) fn of(conn: &quinn::Connection, at: Instant) -> Self {
        let s = conn.stats();
        Self {
            at,
            rtt: s.path.rtt,
            cwnd: s.path.cwnd,
            congestion_events: s.path.congestion_events,
            lost_packets: s.path.lost_packets,
            sent_packets: s.path.sent_packets,
            tx_bytes: s.udp_tx.bytes,
        }
    }
}

/// The windowed round-trip floor: two buckets, the older dropped when
/// the newer is a bucket old.
#[derive(Debug, Default, Clone, Copy)]
struct Floor {
    old: Option<Duration>,
    new: Option<(Instant, Duration)>,
}

impl Floor {
    fn observe(&mut self, at: Instant, rtt: Duration) -> Duration {
        match self.new {
            Some((since, min)) if at.duration_since(since) < FLOOR_BUCKET => {
                self.new = Some((since, min.min(rtt)));
            }
            Some((_, min)) => {
                self.old = Some(min);
                self.new = Some((at, rtt));
            }
            None => self.new = Some((at, rtt)),
        }
        let newest = self.new.map_or(rtt, |(_, m)| m);
        self.old.map_or(newest, |o| o.min(newest))
    }
}

/// The phase machine over successive samples (see the module docs).
#[derive(Debug, Default)]
pub(super) struct QuicPath {
    prev: Option<Sample>,
    phase: PathPhase,
    floor: Floor,
}

impl QuicPath {
    /// The path as of `s`.
    pub(super) fn on_sample(&mut self, s: Sample) -> PathState {
        let floor = self.floor.observe(s.at, s.rtt);
        let capacity = capacity(s.cwnd, s.rtt);
        let mut state = PathState {
            rtt: Some(s.rtt),
            queue_delay: Some(s.rtt.saturating_sub(floor)),
            ..Default::default()
        };
        if let Some(p) = self.prev {
            let secs = s.at.duration_since(p.at).as_secs_f64();
            let sent = s.sent_packets.saturating_sub(p.sent_packets);
            let lost = s.lost_packets.saturating_sub(p.lost_packets);
            if secs > 0.0 {
                let bytes = s.tx_bytes.saturating_sub(p.tx_bytes);
                state.demand = Some(saturate(bytes as f64 / secs));
            }
            state.loss_permille = (lost.min(sent) * 1000).checked_div(sent).map(|p| p as u16);
            let signal = s.congestion_events > p.congestion_events;
            self.phase = match (self.phase, signal) {
                (PathPhase::Open, false) => PathPhase::Open,
                (PathPhase::Open, true) => PathPhase::Suspect,
                (PathPhase::Suspect, false) => PathPhase::Open,
                (PathPhase::Suspect | PathPhase::Paced, true) => PathPhase::Paced,
                (PathPhase::Paced, false) => {
                    let demand = u64::from(state.demand.unwrap_or(0));
                    if u64::from(capacity) * 4 >= demand * 5 {
                        PathPhase::Open
                    } else {
                        PathPhase::Paced
                    }
                }
            };
        }
        self.prev = Some(s);
        state.phase = self.phase;
        state.rate = (self.phase == PathPhase::Paced).then_some(capacity);
        state
    }
}

/// The window's bytes per second: `cwnd` per round trip (a round trip
/// under a millisecond counts as one).
fn capacity(cwnd: u64, rtt: Duration) -> u32 {
    let rtt = rtt.max(Duration::from_millis(1)).as_secs_f64();
    saturate(cwnd as f64 / rtt)
}

fn saturate(v: f64) -> u32 {
    v.clamp(0.0, f64::from(u32::MAX)) as u32
}
