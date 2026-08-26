//! The human-readable report and the single scriptable RESULT line.


use gsb_core::metrics::{
    MetricReport,
    RoomReport, FINE_HIST_BINS, HIST_BINS,
};

use super::*;
use crate::client::*;
use crate::stats::*;

mod result;
pub(crate) use result::*;

/// Extra facts of a separate-process (orchestrated) run; `None` for the
/// in-process and external-direct modes.
pub(crate) struct SepInfo {
    pub(crate) procs: u32,
    pub(crate) server_pid: u32,
    pub(crate) client_pids: Vec<u32>,
    /// The disjoint core sets (`taskset` masks), for the record: e.g.
    /// `server:0,1,2,3;client0:4,5,6,7;client1:8,9,10,11` — or `none`
    /// when pinning was not possible.
    pub(crate) affinity: String,
    /// CPU seconds the server process used over the run (from
    /// /proc/<pid>/stat) — the isolation proof: with disjoint masks,
    /// server CPU cannot hide behind client decode.
    pub(crate) server_cpu_s: f64,
    pub(crate) clients_cpu_s: f64,
}

/// Total room membership in a report: the SUM over all rooms. For a
/// single room (every non-sharded strategy) this is that room's member
/// count; for a sharded room it is the room's total population (the
/// shards partition the room's connections).
pub(crate) fn report_members(report: &MetricReport) -> u32 {
    report.rooms.iter().map(|r| r.members).sum()
}

/// The report's cumulative step count as a "latest report" proxy: the MAX
/// over all rooms. For a single room that room's steps; for a sharded room
/// the shards step in lockstep (one global ticker) so any shard's count
/// marks the report's recency.
pub(crate) fn report_steps(report: &MetricReport) -> u64 {
    report.rooms.iter().map(|r| r.steps).max().unwrap_or(0)
}

/// Fold a report's rooms into ONE [`RoomReport`] so the (single-room-shaped)
/// print code works for both: a single room (identity fold) and a sharded
/// room (N shard reports). The fold is exact for the single-room case
/// (max/sum/min of one element = that element):
/// - cumulative counters (steps, dropped, snapshots, records, joins, …): SUM
///   (the room's total);
/// - worst-case gauges (step_max, late_max, snap_bytes_max, max_group): MAX
///   (the bottleneck shard);
/// - rates: MIN hz (the slowest shard is the room's rate — the shards step
///   together, so a lagging shard drags the room);
/// - `step_hist`: element-wise SUM (the union of all shards' step
///   distributions, so over-budget % and percentiles are room-wide);
/// - `step_mean_us`: weighted by steps (exact for one shard).
pub(crate) fn fold_rooms(report: &MetricReport) -> Option<RoomReport> {
    let first = report.rooms.first()?;
    if report.rooms.len() == 1 {
        return Some(*first);
    }
    let mut acc = *first;
    let mut total_steps: u128 = 0;
    let mut weighted_mean: f64 = 0.0;
    for r in &report.rooms {
        let s = r.steps as f64;
        total_steps += r.steps as u128;
        weighted_mean += r.step_mean_us * s;
        acc.steps = acc.steps.max(r.steps);
        acc.hz = acc.hz.min(r.hz);
        acc.step_min_us = acc.step_min_us.min(r.step_min_us);
        acc.step_max_us = acc.step_max_us.max(r.step_max_us);
        for i in 0..HIST_BINS {
            acc.step_hist[i] += r.step_hist[i];
        }
        for i in 0..FINE_HIST_BINS {
            acc.step_fine_hist[i] += r.step_fine_hist[i];
        }
        acc.late_max_us = acc.late_max_us.max(r.late_max_us);
        acc.lagged_events += r.lagged_events;
        acc.lagged_ticks += r.lagged_ticks;
        acc.dropped += r.dropped;
        acc.dropped_actions += r.dropped_actions;
        acc.keepalive_resends += r.keepalive_resends;
        acc.snapshots += r.snapshots;
        acc.snap_bytes_max = acc.snap_bytes_max.max(r.snap_bytes_max);
        acc.snap_overflows += r.snap_overflows;
        acc.snap_records += r.snap_records;
        acc.shipped_bytes += r.shipped_bytes;
        acc.groups += r.groups;
        acc.members += r.members;
        acc.max_group = acc.max_group.max(r.max_group);
        acc.joins += r.joins;
        acc.leaves += r.leaves;
        // Reconnect counters (§10): cumulative like joins/leaves → SUM.
        // `detached` is an instant gauge → MAX (the worst shard's park
        // population), mirroring the other gauges above.
        acc.detached = acc.detached.max(r.detached);
        acc.resumes += r.resumes;
        acc.resume_rejected_stale += r.resume_rejected_stale;
        acc.detach_expired_despawn += r.detach_expired_despawn;
        acc.detach_expired_ai += r.detach_expired_ai;
    }
    acc.step_mean_us = if total_steps > 0 {
        weighted_mean / total_steps as f64
    } else {
        0.0
    };
    Some(acc)
}
