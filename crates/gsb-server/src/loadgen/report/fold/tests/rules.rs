//! One test per fold rule. The fixture (three shard reports whose every
//! field differs) is the parent module's; these are the assertions.

use super::*;

/// THE rule-table test: one assertion per field of [`RoomReport`], in
/// declaration order, against the rule documented next to the fold.
#[test]
fn folding_shards_applies_one_rule_per_field() {
    let f = fold_rooms(&three_shards()).expect("three shard reports fold");

    // NOT MERGED — an identity, not a measurement.
    assert_eq!(f.room, RoomId(1 << 16), "the folded row keeps the first id");

    // MAX — the shards tick in lockstep, so the room's step count is one
    // shard's, not their sum.
    assert_eq!(f.steps, 300, "steps");
    // MIN over the shards that reported a rate.
    close(f.hz, 28.0, "hz");
    // Configuration, not a measurement: MIN (see the fold's rule table).
    assert_eq!(f.budget_us, 33_333, "budget_us");

    // Extremes: MIN and MAX, and neither sits on the first or last shard.
    assert_eq!(f.step_min_us, 25, "step_min_us");
    close(
        f.step_mean_us,
        13_000.0 / 600.0,
        "step_mean_us (steps-weighted)",
    );
    assert_eq!(f.step_max_us, 900, "step_max_us");

    // Element-wise SUM, each shard counted exactly once.
    assert_eq!(f.step_hist[0], 594, "step_hist[0]");
    assert_eq!(f.step_hist[HIST_OVERFLOW_BIN], 6, "step_hist overflow bin");
    assert_eq!(
        f.step_hist.iter().sum::<u64>(),
        TOTAL_STEPS,
        "the folded log2 histogram bins every shard's every step, once"
    );
    assert_eq!(f.step_fine_hist[1], 200, "step_fine_hist[1]");
    assert_eq!(f.step_fine_hist[2], 100, "step_fine_hist[2]");
    assert_eq!(f.step_fine_hist[9], 300, "step_fine_hist[9]");

    assert_eq!(f.late_min_us, 700, "late_min_us");
    close(
        f.late_mean_us,
        130_000.0 / 600.0,
        "late_mean_us (steps-weighted — a mean of means is not a mean)",
    );
    assert_eq!(f.late_max_us, 9_000, "late_max_us");

    // SUM — cumulative counters.
    assert_eq!(f.lagged_events, 6, "lagged_events");
    assert_eq!(f.lagged_ticks, 60, "lagged_ticks");
    assert_eq!(f.dropped, 9, "dropped");
    // SUM — a per-shard rate over disjoint counters; averaging would
    // report a quarter of the room's loss.
    close(f.dropped_s, 7.0, "dropped_s");
    assert_eq!(f.keepalive_resends, 18, "keepalive_resends");
    assert_eq!(f.snapshots, 600, "snapshots");
    close(f.snap_bytes_s, 7_000.0, "snap_bytes_s");
    assert_eq!(f.snap_bytes_max, 1_200, "snap_bytes_max");
    assert_eq!(f.snap_overflows, 3, "snap_overflows");
    assert_eq!(f.snap_records, 6_000, "snap_records");
    assert_eq!(f.shipped_bytes, 60_000, "shipped_bytes");
    close(f.shipped_s, 600.0, "shipped_s");
    assert_eq!(f.shipped_frames, 150, "shipped_frames");
    assert_eq!(f.private_frames, 15, "private_frames");

    // SUM — the shards PARTITION the room's population.
    assert_eq!(f.groups, 6, "groups");
    assert_eq!(f.members, 50, "members");
    assert_eq!(f.max_group, 9, "max_group (an extremum, not a population)");
    assert_eq!(f.joins, 50, "joins");
    assert_eq!(f.leaves, 6, "leaves");
    assert_eq!(
        f.detached, 9,
        "detached is a partition gauge like members, so it SUMs (it used \
         to take the worst shard's park count)"
    );
    assert_eq!(f.resumes, 6, "resumes");
    assert_eq!(f.resume_rejected_stale, 15, "resume_rejected_stale");
    assert_eq!(f.detach_expired_despawn, 24, "detach_expired_despawn");
    assert_eq!(f.detach_expired_ai, 3, "detach_expired_ai");
    assert_eq!(f.detach_forced, 3, "detach_forced");
    assert_eq!(f.effects_applied, 70, "effects_applied");
    assert_eq!(f.effects_forwarded, 6, "effects_forwarded");
    assert_eq!(f.effects_orphaned, 2, "effects_orphaned");
    assert_eq!(f.effects_dropped, 3, "effects_dropped");
    assert_eq!(f.effects_refused, 9, "effects_refused");
    assert_eq!(f.migrations_out, 18, "migrations_out");
    assert_eq!(f.migrations_in, 18, "migrations_in");
    assert_eq!(f.migrations_failed, 4, "migrations_failed");
    assert_eq!(f.team_exports, 89, "team_exports");
    assert_eq!(f.team_export_drops, 1, "team_export_drops");
    assert_eq!(f.team_export_records, 930, "team_export_records");
    assert_eq!(f.team_over_cap, 2, "team_over_cap");
    assert_eq!(f.team_imports, 183, "team_imports");
    assert_eq!(f.team_import_records, 2_730, "team_import_records");
    assert_eq!(f.team_expired, 1, "team_expired");
    assert_eq!(f.team_over_budget, 4, "team_over_budget");

    // SUM — the RPC family, none of which was folded at all before.
    assert_eq!(f.requests_local, 60, "requests_local");
    assert_eq!(f.requests_external, 6, "requests_external");
    assert_eq!(
        f.requests_rejected_malformed, 1,
        "requests_rejected_malformed"
    );
    assert_eq!(f.requests_rejected_dup, 2, "requests_rejected_dup");
    assert_eq!(
        f.requests_rejected_no_handler, 3,
        "requests_rejected_no_handler"
    );
    assert_eq!(f.requests_rejected_logic, 4, "requests_rejected_logic");
    assert_eq!(
        f.requests_rejected_conn_cap, 5,
        "requests_rejected_conn_cap"
    );
    assert_eq!(
        f.requests_rejected_room_cap, 6,
        "requests_rejected_room_cap"
    );
    assert_eq!(f.requests_timed_out, 24, "requests_timed_out");
    assert_eq!(f.requests_late, 7, "requests_late");
    assert_eq!(
        f.pending_requests, 15,
        "pending_requests (a gauge, but a partitioned one)"
    );
    assert_eq!(f.metrics_dropped, 14, "metrics_dropped");
    // The logic's own counters, each by the rule it carries.
    assert_eq!(f.logic.get("kills"), Some(15), "a SUM counter adds");
    assert_eq!(
        f.logic.get("fights_peak"),
        Some(9),
        "a MAX counter takes the middle shard's peak"
    );
    assert_eq!(
        f.logic.get("late_name"),
        Some(1),
        "a one-shard name is kept"
    );
    assert_eq!(f.logic.dropped(), 1, "the overflow counts add");
}

/// The accumulator used to be seeded with the first element and then run
/// over the WHOLE slice, so shard 0 landed in every SUM twice. A 4-shard
/// 50-client run reported 61 members.
#[test]
fn the_first_shard_is_absorbed_exactly_once() {
    let mut a = shard(0);
    let mut b = shard(1);
    a.members = 11;
    b.members = 39;
    a.snap_records = 1_000;
    b.snap_records = 2_000;

    let f = fold_rooms(&report(vec![a, b])).expect("two shard reports fold");

    assert_eq!(f.members, 50, "11 + 39 — not 11 + (11 + 39)");
    assert_eq!(f.snap_records, 3_000, "1000 + 2000 — not 1000 + 3000");
    assert_eq!(
        f.step_hist.iter().sum::<u64>(),
        a.steps + b.steps,
        "the histogram covers each shard's steps once"
    );
}

/// A percentile over the FOLDED fine histogram must be taken against the
/// folded histogram's population (the shards' steps SUMMED), not against
/// `RoomReport::steps` (the room's tick count, folded with MAX). The
/// load generator's `step_p50_fine_us` / `step_p90_fine_us` handed it
/// `steps`, so on a 4-shard room it printed roughly the p12.5 under a
/// p50 label.
#[test]
fn a_percentile_over_the_folded_histogram_needs_the_folded_population() {
    let f = fold_rooms(&three_shards()).expect("three shard reports fold");

    assert_eq!(folded_steps(&f), TOTAL_STEPS, "the histogram's population");
    assert_eq!(f.steps, 300, "the room's tick count is NOT that population");

    let right = fine_hist_percentile_us(&f.step_fine_hist, folded_steps(&f), 50);
    let wrong = fine_hist_percentile_us(&f.step_fine_hist, f.steps, 50);
    assert_eq!(
        right,
        Some(16),
        "p50 over the 600 steps the histogram holds"
    );
    assert_eq!(wrong, Some(8), "p50 mis-taken over 300 — a lower bin");
    assert_ne!(right, wrong, "the two denominators must not be confused");

    // And the pairing the report lines actually use.
    assert_eq!(
        fine_percentiles_us(&f),
        (16, 72),
        "step_p50_fine_us / step_p90_fine_us over the folded population"
    );
    let naive = |q| fine_hist_percentile_us(&f.step_fine_hist, f.steps, q);
    assert_eq!(
        (naive(50), naive(90)),
        (Some(8), Some(16)),
        "what the lines printed while they passed the tick count"
    );
}

/// `hz = 0.0` means "this shard emitted no sample in this window" (see
/// `RoomReport::hz`), not "this shard stopped". A plain `min` would let
/// one quiet shard report the whole room as stopped.
#[test]
fn folding_rates_skips_the_windows_that_carried_no_sample() {
    let (mut a, mut b, mut c) = (shard(0), shard(1), shard(2));
    a.hz = 30.0;
    b.hz = 0.0;
    c.hz = 28.5;
    let f = fold_rooms(&report(vec![a, b, c])).expect("fold");
    close(f.hz, 28.5, "the slowest shard that actually reported");

    a.hz = 0.0;
    c.hz = 0.0;
    let f = fold_rooms(&report(vec![a, b, c])).expect("fold");
    close(f.hz, 0.0, "no shard reported a rate in this window");
}

/// `budget_us` is CONFIGURATION, not a measurement: the shards of a room
/// share one `RoomConfig`, so they always agree. If they ever do not, the
/// fold takes the SMALLER — it is the denominator of the overflow
/// fraction and the histogram's edges, and the smaller budget reads
/// overflow earlier (the safe direction for a threshold).
#[test]
fn folding_a_disagreeing_budget_takes_the_smaller() {
    let (mut a, mut b) = (shard(0), shard(1));
    a.budget_us = 33_333;
    b.budget_us = 16_666;
    let f = fold_rooms(&report(vec![a, b])).expect("fold");
    assert_eq!(f.budget_us, 16_666);
}

/// A single-room (non-sharded) report is the identity fold: the one
/// element comes back untouched, every field included.
#[test]
fn folding_one_room_is_the_identity() {
    let only = shard(1);
    let f = fold_rooms(&report(vec![only])).expect("one room folds");
    assert_eq!(f.room, only.room);
    assert_eq!(f.steps, only.steps);
    assert_eq!(f.step_min_us, only.step_min_us);
    assert_eq!(f.late_min_us, only.late_min_us);
    assert_eq!(f.members, only.members);
    assert_eq!(f.detached, only.detached);
    assert_eq!(f.pending_requests, only.pending_requests);
    assert_eq!(f.requests_local, only.requests_local);
    assert_eq!(f.logic, only.logic);
    close(f.late_mean_us, only.late_mean_us, "late_mean_us");
    assert_eq!(
        folded_steps(&f),
        only.steps,
        "one actor bins every step exactly once, so the histogram's \
         population is its step count"
    );
    assert!(
        fold_rooms(&report(Vec::new())).is_none(),
        "no rooms, no fold"
    );
}

/// The two sibling merge helpers: membership SUMs over the report's
/// rooms, recency takes the MAX step count.
#[test]
fn the_report_level_helpers_sum_members_and_max_steps() {
    let r = three_shards();
    assert_eq!(report_members(&r), 50, "the room's total population");
    assert_eq!(report_steps(&r), 300, "the freshest shard's step count");
}
