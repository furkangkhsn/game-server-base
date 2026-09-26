//! A sharded game's population per shard: the per-shard sample rows the
//! fold (`fold.rs`) sums into one room, read back apart — how the
//! players spread across the shards.
//!
//! ## Only a consistent cut is a population (F18)
//!
//! A report is the collector's LATEST sample of every producer, and the
//! shard actors send theirs independently (each at the end of its own
//! step, every `metrics_every` steps). The collector emits on its own
//! tick, so a report can land between two shards' samples of the same
//! round: some rows are round `k`, the others still round `k − 1`. Each
//! row is exact for its shard at its own sample tick; their SUM is not
//! the room at any instant. A player who moved from an old-round shard
//! to a new-round shard between the two rounds is in both rows (the
//! room reads one too many); one who moved the other way is in neither.
//!
//! The engine's own guarantee is per tick index: a migrating player
//! leaves its source's members at the commit tick `h` and joins its
//! destination's at the install, `h + 1` at the earliest — so rows
//! sampled on the same tick (or one tick apart) never count a player
//! twice; the price is the in-flight player counted by neither
//! (`docs/CROSS-SHARD.md` §4d, `docs/DESIGN.md` §12).
//!
//! So a report's rows are summed as one population only when they are a
//! CONSISTENT CUT: every row sampled at the same step with the same
//! missed ticks (`steps`, `lagged_ticks`) — the shards step in lockstep
//! off one ticker and sample on the same step multiples, so equal pairs
//! are the same round, the same tick index (up to the one tick two
//! shards' ticker subscriptions can differ by, which the install gate
//! absorbs). A single room's report is one row: always a cut.

use gsb_core::metrics::MetricReport;

use super::*;

#[cfg(test)]
mod tests;

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

/// Whether `report`'s rows are one instant of the room (see the module
/// docs): non-empty, and every row at the same `(steps, lagged_ticks)`.
pub(crate) fn consistent_cut(report: &MetricReport) -> bool {
    let mut rows = report.rooms.iter().map(|r| (r.steps, r.lagged_ticks));
    rows.next().is_some_and(|first| rows.all(|r| r == first))
}

/// Whether the run has any consistent cut. A run without one (a shard
/// that lagged unevenly never lines up with the others again) is read
/// from its torn reports, as every run was before F18 — and the human
/// block says so.
pub(crate) fn has_consistent_cut(reports: &[MetricReport]) -> bool {
    reports.iter().any(consistent_cut)
}

/// The reports a population is read from: the consistent cuts, or —
/// a run with none — every non-empty report (see [`has_consistent_cut`]).
fn population_reports(reports: &[MetricReport]) -> impl Iterator<Item = &MetricReport> + Clone {
    let cuts_only = has_consistent_cut(reports);
    reports
        .iter()
        .filter(move |r| !r.rooms.is_empty() && (!cuts_only || consistent_cut(r)))
}

/// The run's full population: the largest member total over the
/// population reports. A torn report's double count cannot win it, and
/// a double count that persists over consistent cuts still does.
pub(crate) fn peak_population(reports: &[MetricReport]) -> u32 {
    population_reports(reports)
        .map(report_members)
        .max()
        .unwrap_or(0)
}

/// The first and the last population report that carry the run's full
/// population (`peak_members`, the steady state — the same window the
/// overlap measurement uses): the spread early and late in the run.
pub(crate) fn steady_span(
    reports: &[MetricReport],
    peak_members: u32,
) -> Option<(&MetricReport, &MetricReport)> {
    let steady = population_reports(reports).filter(move |r| report_members(r) == peak_members);
    Some((
        steady.clone().min_by_key(|r| report_steps(r))?,
        steady.max_by_key(|r| report_steps(r))?,
    ))
}
