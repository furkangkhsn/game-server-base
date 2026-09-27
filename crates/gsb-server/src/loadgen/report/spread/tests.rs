//! The population a sharded room's reports can be read from (F18): a
//! report whose shard rows come from different sample rounds is torn,
//! and a torn report must not become the run's population or its
//! spread.

use gsb_core::id::RoomId;
use gsb_core::metrics::{MetricReport, RoomReport};

use super::{
    consistent_cut, has_populated_cut, members_by_row, peak_population, steady_end, steady_span,
};
use crate::report::fold::tests::{report, shard};

/// Shard `index`'s row of room 1: sampled at step `steps` (with
/// `lagged` missed ticks), holding `members`. Every other field is the
/// fold fixture's — none of them takes part in the population.
fn row(index: u64, steps: u64, lagged: u64, members: u32) -> RoomReport {
    let mut r = shard(0);
    r.room = RoomId((1 << 16) + index);
    r.steps = steps;
    r.lagged_ticks = lagged;
    r.members = members;
    r
}

/// One report of the four-shard room: `(steps, members)` per shard, in
/// shard order, none of them lagged.
fn cut(rows: [(u64, u32); 4]) -> MetricReport {
    report(
        rows.iter()
            .enumerate()
            .map(|(i, &(steps, members))| row(i as u64, steps, 0, members))
            .collect(),
    )
}

/// The report stream of a real failing run (8 bots, 4 shards, a
/// 30-process CPU hog; `loadgen_drives_the_mmo`, F18), row for row:
/// shard, sampled step and members. Report 1 is TORN — the collector
/// emitted it after shards 0 and 2 had sent their step-60 samples but
/// before shards 1 and 3 had, so a player who moved from shard 1 to
/// shard 0 between steps 30 and 60 is in shard 0's new row AND shard 1's
/// old one: 3+2+2+2 = 9 for 8 players. The step-90 report is a
/// consistent cut with one player in flight (7 — a migration committed
/// on its sample tick; the destination installs it one tick later).
fn failing_run() -> Vec<MetricReport> {
    let mut first = cut([(30, 2), (30, 2), (30, 0), (30, 2)]);
    first.rooms.remove(2); // shard 2 had not sampled yet: not a cut (B45)
    vec![
        first,
        cut([(60, 3), (30, 2), (60, 2), (30, 2)]),
        cut([(90, 2), (90, 1), (90, 3), (90, 1)]),
        cut([(120, 2), (120, 1), (120, 4), (120, 1)]),
        cut([(120, 2), (120, 1), (120, 4), (120, 1)]),
    ]
}

/// The failing run itself: the torn report's 9 used to be the "peak",
/// and with it the whole steady window, so the RESULT line printed
/// `shard_members=3,2,2,2` for 8 players. Only a consistent cut is a
/// population.
#[test]
fn a_torn_report_is_neither_the_population_nor_the_spread() {
    let reports = failing_run();
    assert_eq!(
        peak_population(&reports, 4),
        8,
        "the torn report's double count must not become the population"
    );
    let (first, last) = steady_span(&reports, 8, 4).expect("a steady window");
    assert_eq!(members_by_row(first), "2,1,4,1");
    assert_eq!(members_by_row(last), "2,1,4,1");
}

/// A double count that PERSISTS is still visible: when every report of
/// the run is a consistent cut summing to 9 players, the population is
/// 9 — the rule drops torn reports, it does not clamp the sum.
#[test]
fn a_consistent_overcount_is_still_reported() {
    let reports = vec![
        cut([(30, 3), (30, 2), (30, 2), (30, 2)]),
        cut([(60, 3), (60, 2), (60, 2), (60, 2)]),
    ];
    assert_eq!(peak_population(&reports, 4), 9);
    let (_, last) = steady_span(&reports, 9, 4).expect("a steady window");
    assert_eq!(members_by_row(last), "3,2,2,2");
}

/// Equal steps are not enough when the shards lagged differently: a
/// shard that missed ticks samples its step `n` that many ticks LATER
/// than the others, so its row is not the same instant as theirs.
#[test]
fn rows_of_unevenly_lagged_shards_are_not_one_instant() {
    let torn = report(vec![
        row(0, 60, 5, 3),
        row(1, 60, 0, 2),
        row(2, 60, 0, 2),
        row(3, 60, 0, 2),
    ]);
    let even = report(vec![
        row(0, 90, 5, 2),
        row(1, 90, 5, 2),
        row(2, 90, 5, 2),
        row(3, 90, 5, 2),
    ]);
    assert_eq!(peak_population(&[torn, even], 4), 8);
}

/// A single room's report is one row: always one instant.
#[test]
fn a_single_room_report_is_always_a_population() {
    let mut one = shard(1);
    one.members = 13;
    assert_eq!(peak_population(&[report(vec![one])], 1), 13);
}

/// A run with NO consistent cut (a shard lagged unevenly and never lined
/// up again) still reports: from the torn rows, as before F18 — the
/// human block names it (see `population_reports`).
#[test]
fn a_run_without_a_consistent_cut_falls_back_to_every_report() {
    let torn = report(vec![row(0, 60, 5, 3), row(1, 60, 0, 2)]);
    assert_eq!(peak_population(&[torn], 2), 5);
}

/// A cut holds EVERY shard's row (B45). A report the collector emitted
/// before every shard had sampled lacks rows; its rows can agree on
/// `(steps, lagged_ticks)` and still not be the room — the missing
/// shard's players are in no row.
#[test]
fn a_report_missing_a_shard_row_is_not_a_cut() {
    let partial = report(vec![row(1, 30, 0, 2), row(2, 30, 0, 2), row(3, 30, 0, 2)]);
    assert!(!consistent_cut(&partial, 4), "shard 0 has no row");
    let whole = cut([(30, 2), (30, 2), (30, 2), (30, 2)]);
    assert!(consistent_cut(&whole, 4));
}

/// A run whose only equal-rowed report is PARTIAL has no consistent cut:
/// its population and spread come from the torn fallback (and the human
/// block says so), not from the partial report — which used to be taken
/// as the run's only cut: population 6 for 8 players, and a three-shard
/// `shard_members=2,2,2` for a four-shard room.
#[test]
fn a_partial_report_does_not_stand_in_for_the_population() {
    let reports = vec![
        report(vec![row(1, 30, 0, 2), row(2, 30, 0, 2), row(3, 30, 0, 2)]),
        cut([(60, 3), (30, 2), (60, 1), (30, 2)]),
        cut([(90, 2), (60, 2), (90, 2), (60, 2)]),
    ];
    assert!(!has_populated_cut(&reports, 4), "the run is torn");
    assert_eq!(peak_population(&reports, 4), 8);
    let (first, last) = steady_span(&reports, 8, 4).expect("a steady window");
    assert_eq!(members_by_row(first), "3,2,1,2");
    assert_eq!(members_by_row(last), "2,2,2,2");
}

/// The overlap window's END is a population report too (B46). A torn
/// report can sum to the peak (a double count and an in-flight player
/// cancel out) and so can a partial one (the missing shard's players
/// summed twice elsewhere); either, being LATER than every cut, used to
/// end the window — its rows are no instant of the room, and its
/// `snap_records` are shards at different steps. The end is the last
/// consistent cut at the peak.
#[test]
fn a_torn_or_partial_report_at_the_peak_does_not_end_the_steady_window() {
    let mut partial = cut([(120, 4), (120, 2), (120, 2), (120, 0)]);
    partial.rooms.remove(3); // shard 3 has not sampled yet: not a cut (B45)
    let reports = vec![
        cut([(30, 2), (30, 2), (30, 2), (30, 2)]),
        cut([(60, 3), (60, 1), (60, 2), (60, 2)]),
        cut([(90, 3), (90, 1), (60, 2), (60, 2)]), // torn, sums to 8
        partial,                                   // partial, sums to 8
    ];
    assert_eq!(peak_population(&reports, 4), 8);
    let end = steady_end(&reports, 8, 4).expect("a window end");
    assert_eq!(
        members_by_row(end),
        "3,1,2,2",
        "the window ends on the last consistent cut (step 60)"
    );
    assert!(consistent_cut(end, 4));
}

/// A run with no consistent cut ends its window on the torn reports, the
/// same fallback its population takes (`population_reports`).
#[test]
fn without_a_cut_the_window_end_falls_back_to_every_report() {
    let reports = vec![
        cut([(60, 3), (30, 2), (60, 1), (30, 2)]),
        cut([(90, 2), (60, 2), (90, 2), (60, 2)]),
    ];
    assert!(!has_populated_cut(&reports, 4));
    let end = steady_end(&reports, 8, 4).expect("a window end");
    assert_eq!(members_by_row(end), "2,2,2,2");
}

mod empty;
