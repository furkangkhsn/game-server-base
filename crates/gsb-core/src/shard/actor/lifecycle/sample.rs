//! The metrics sample: the shard's counters, gauges and border
//! accounting, snapshotted once per report period.

use crate::id::RoomId;
use crate::metrics::{LogicCounters, RoomSample};
use crate::shard::actor::ShardActor;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip rides every exchange and view; the bounds mirror what
    // the delta protocol does with it (diff via PartialEq, clone into
    // each neighbor's message, store in the actor's maps).
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Build this shard's metrics sample. The sample id is the logical
    /// room id shifted into the shard sub-space (`room << 16 | index` —
    /// see module docs, "Metrics identity") so every shard is its own
    /// report line (the collector keys by the sample id).
    pub(crate) fn sample(&self) -> RoomSample {
        let fx = &self.effects.stats;
        RoomSample {
            room: RoomId(self.config.id.0.saturating_mul(1 << 16) + self.index as u64),
            emit_at: Instant::now(),
            steps: self.steps,
            budget_us: self.budget_us,
            lagged_events: self.m.lagged_events,
            lagged_ticks: self.m.lagged_ticks,
            step_min_us: self.m.step_min_us,
            step_max_us: self.m.step_max_us,
            step_sum_us: self.m.step_sum_us,
            step_hist: self.m.step_hist,
            step_fine_hist: self.m.step_fine_hist,
            late_min_us: self.m.late_min_us,
            late_max_us: self.m.late_max_us,
            late_sum_us: self.m.late_sum_us,
            dropped_frames: self.m.dropped_frames,
            keepalive_resends: self.m.keepalive_resends,
            snapshots: self.m.snapshots,
            snap_bytes: self.m.snap_bytes,
            snap_bytes_max: self.m.snap_bytes_max,
            snap_overflows: self.m.snap_overflows,
            snap_records: self.m.snap_records,
            shipped_bytes: self.m.shipped_bytes,
            shipped_frames: self.m.shipped_frames,
            private_frames: self.m.private_frames,
            joins: self.m.joins,
            leaves: self.m.leaves,
            detached: self.conns.values().filter(|rc| rc.detached).count() as u32,
            resumes: self.m.resumes,
            resume_rejected_stale: self.m.resume_rejected_stale,
            detach_expired_despawn: self.m.detach_expired_despawn,
            detach_expired_ai: self.m.detach_expired_ai,
            detach_forced: self.m.detach_forced,
            // The effect counters' operator-facing five (RoomSample docs);
            // the full split is the `remote_effect_summary` log line.
            effects_applied: fx.applied,
            effects_forwarded: fx.forwarded,
            effects_orphaned: fx.orphaned,
            effects_dropped: fx.dropped_full + fx.dropped_closed + fx.dropped_hops + fx.expired,
            effects_refused: fx.refused,
            migrations_out: self.m.migrations_out,
            migrations_in: self.m.migrations_in,
            migrations_failed: self.m.migrations_failed,
            // The team exchange's counters, cumulative (the log line is
            // their ~1 s window).
            team_exports: self.tstats.exports,
            team_export_drops: self.tstats.export_drops,
            team_export_records: self.tstats.export_records,
            team_over_cap: self.tstats.over_cap,
            team_over_budget: self.tstats.over_budget,
            team_imports: self.tstats.imports,
            team_import_records: self.tstats.import_records,
            team_expired: self.tstats.expired,
            // Faz 3: this shard runs the RPC machinery (the room actor's
            // counters, mirrored one-to-one) — no longer pinned to zero.
            requests_local: self.m.requests_local,
            requests_external: self.m.requests_external,
            requests_rejected_malformed: self.m.requests_rejected_malformed,
            requests_rejected_dup: self.m.requests_rejected_dup,
            requests_rejected_no_handler: self.m.requests_rejected_no_handler,
            requests_rejected_logic: self.m.requests_rejected_logic,
            requests_rejected_conn_cap: self.m.requests_rejected_conn_cap,
            requests_rejected_room_cap: self.m.requests_rejected_room_cap,
            requests_refused_congested: self.m.requests_refused_congested,
            requests_dropped_unread: self.m.requests_dropped_unread,
            requests_timed_out: self.m.requests_timed_out,
            requests_late: self.m.requests_late,
            pending_requests: self.pending_total as u32,
            groups: self.groups.len() as u32,
            members: self.conns.len() as u32,
            max_group: self.m.step_max_group,
            metrics_dropped: self.m.metrics_dropped,
            logic: self.logic_counters(),
        }
    }

    /// The logic's own counters for this sample (F9): whatever it puts
    /// into an empty set — nothing, by default.
    fn logic_counters(&self) -> LogicCounters {
        let mut out = LogicCounters::new();
        self.logic.logic_counters(&self.world, &mut out);
        out
    }
}
