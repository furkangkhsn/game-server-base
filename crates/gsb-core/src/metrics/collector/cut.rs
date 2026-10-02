//! The periodic emit waits for a sharded room's round to land (BACKLOG
//! F29): a report is a consistent cut of every sharded room whose shards
//! keep their schedule.
//!
//! The collector subscribes to the same ticker the shards step on, and
//! the shards send their samples while stepping a tick — each shard on
//! its own task. On the tick a report falls due, the collector's drain
//! can land between them: some rows already carry round `k`, the others
//! still `k − 1`, and the report is torn (DESIGN §12 "tutarlı kesit": a
//! migrating player counts twice or not at all in the sum). How often
//! depends on where the emit falls against the shards' sample tick: a
//! shard slowed below its rate walks its sample phase across the
//! report's, and under load the shards' sends of one round spread over
//! tens of milliseconds.
//!
//! So a due report waits while a round is in flight
//! (`MetricAccumulator::round_in_flight`): the drain repeats on every
//! following tick and the report goes out on the first one that finds
//! the rows lined up — normally the next tick, the shards' sends of one
//! round are one tick's work. The wait is bounded by [`CUT_GRACE`] from
//! the due time (capped at half the period): a shard that died or stalls
//! cannot hold the reports back; at the grace the report goes out torn,
//! as it did before, and the consumer finds it torn the way it always
//! has (`(steps, lagged_ticks)` apart). The next report is due one
//! period after the one that went out — a wait moves the emit just past
//! the shards' round, where the following reports land without waiting.
//!
//! Only a disagreement waiting can cure counts: rows apart on
//! `lagged_ticks` never line up again (BACKLOG F20) and do not hold a
//! report. Single rooms (one row) are always a cut. The final report
//! does not wait here: it waits for the producers' last words instead
//! (`closing`).

use std::time::{Duration, Instant};

use super::MetricsCollector;

/// How long a due report waits, at most, for a sharded room's round in
/// flight to land (see the module docs), from the time it fell due;
/// capped at half the report period. A round's sends are one tick's work
/// (33 ms at 30 Hz); a quarter of the server's one-second period lets a
/// round spread over several starved ticks land and still keeps every
/// report inside its own period.
pub const CUT_GRACE: Duration = Duration::from_millis(250);

impl MetricsCollector {
    /// Whether the report due since `self.next_report` goes out at `now`
    /// (on the tick clock): no sharded room's round is in flight, or the
    /// grace from the due time has passed (then torn, with a `debug`).
    pub(super) fn ready_to_emit(&self, now: Instant) -> bool {
        if !self.acc.round_in_flight() {
            return true;
        }
        let grace = self.cut_grace.min(self.period / 2);
        let late = now.saturating_duration_since(self.next_report) >= grace;
        if late {
            tracing::debug!(
                ?grace,
                "a sharded room's round was still in flight at the cut grace; \
                 the report goes out torn"
            );
        }
        late
    }
}
