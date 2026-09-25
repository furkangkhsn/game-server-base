//! A sharded game's population per shard: the per-shard sample rows the
//! fold (`fold.rs`) sums into one room, read back apart — how the
//! players spread across the shards.

use gsb_core::metrics::MetricReport;

use super::*;

/// The members of each row of `report`, in sample-id order (a sharded
/// room's rows are `room << 16 | shard`, so this is shard order), joined
/// with `,` — e.g. `31,40,37,42`.
pub(crate) fn members_by_row(report: &MetricReport) -> String {
    let mut rows: Vec<_> = report.rooms.iter().map(|r| (r.room.0, r.members)).collect();
    rows.sort_unstable_by_key(|&(id, _)| id);
    rows.iter()
        .map(|(_, m)| m.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The first and the last report that carry the run's full population
/// (`peak_members`, the steady state — the same window the overlap
/// measurement uses): the spread early and late in the run.
pub(crate) fn steady_span(
    reports: &[MetricReport],
    peak_members: u32,
) -> Option<(&MetricReport, &MetricReport)> {
    let steady = || {
        reports
            .iter()
            .filter(move |r| !r.rooms.is_empty() && report_members(r) == peak_members)
    };
    Some((
        steady().min_by_key(|r| report_steps(r))?,
        steady().max_by_key(|r| report_steps(r))?,
    ))
}
