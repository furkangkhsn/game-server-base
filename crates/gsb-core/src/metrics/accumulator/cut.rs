//! Whether a sharded room's rows are mid-round right now (BACKLOG F29):
//! the question the collector asks before it emits (`collector::cut`).
//!
//! A sharded room reports one row per shard, under the shard's derived
//! id `room << 16 | index` (`crate::shard::sample_id`, the sub-space
//! above real room ids). The shards step in lockstep off one ticker and
//! sample on the same step multiples, so their rows are one round — a
//! consistent cut (DESIGN §12 "tutarlı kesit") — exactly when they agree
//! on `(steps, lagged_ticks)`. Rows that agree on `lagged_ticks` and NOT
//! on `steps` are a round in flight: some shards sent round `k`, the
//! others are about to (they took the same tick and are still stepping
//! it). That is the one disagreement waiting can cure. Rows apart on
//! `lagged_ticks` (a shard missed ticks the others did not, the uneven
//! `Lagged`) sample on different ticks from then on and never line up
//! again: waiting cannot cure that, so it does not count (BACKLOG F20).
//!
//! A report the collector emits torn anyway — at its cut grace — is
//! counted here (F70), so the report carries how often the bound is hit.
//!
//! Only live rows are asked: a destroyed room's rows linger with frozen
//! counters (`ROOM_GONE_GRACE_REPORTS`) and each shard froze at its own
//! last step.

use std::collections::BTreeMap;

use super::MetricAccumulator;
use crate::id::RoomId;

/// The logical room a shard's row belongs to — the inverse of
/// `crate::shard::sample_id` — or `None` for a single room's row (an id
/// below the shard sub-space).
pub(crate) fn sharded_room(row: RoomId) -> Option<RoomId> {
    (row.0 >= 1 << 16).then_some(RoomId(row.0 >> 16))
}

impl MetricAccumulator {
    /// Whether some live sharded room's rows are a round in flight (see
    /// the module docs): they agree on `lagged_ticks` and not on `steps`.
    pub(crate) fn round_in_flight(&self) -> bool {
        // Per room and lagged_ticks: the first row's steps, and whether a
        // later row of the same pair has other steps.
        let mut rounds: BTreeMap<(RoomId, u64), (u64, bool)> = BTreeMap::new();
        let live = self
            .rooms
            .iter()
            .filter(|(id, _)| !self.rooms_gone_grace.contains_key(id));
        for (id, acc) in live {
            let Some(room) = sharded_room(*id) else {
                continue;
            };
            let key = (room, acc.latest.lagged_ticks);
            let (steps, split) = rounds.entry(key).or_insert((acc.latest.steps, false));
            *split |= *steps != acc.latest.steps;
        }
        rounds.values().any(|&(_, split)| split)
    }

    /// The report about to be built goes out torn: the cut grace ran out
    /// with a round still in flight (BACKLOG F70). Counted on that report
    /// and every one after ([`crate::metrics::MetricReport::reports_torn_at_cut_grace`]).
    pub(crate) fn count_torn_report(&mut self) {
        self.reports_torn_at_cut_grace += 1;
    }
}

#[cfg(test)]
mod tests;
