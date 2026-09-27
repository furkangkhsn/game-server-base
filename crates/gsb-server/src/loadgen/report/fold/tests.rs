//! The sharded-room fold, field by field: three DIFFERENT shard reports
//! in, one room report out, every field asserted against the rule its
//! sibling module documents.
//!
//! Identical fixtures are useless here — a fold that returns the first
//! element, the last element, the sum or the maximum all agree when the
//! inputs agree. Every field below therefore differs across the shards,
//! and no extremum sits on shard 0 or on the last shard.

use std::time::Instant;

use gsb_core::id::RoomId;
use gsb_core::metrics::{
    FINE_HIST_BINS, HIST_BINS, HIST_OVERFLOW_BIN, LogicCounter, LogicCounters, MetricReport,
    NetReport, RoomReport, fine_hist_percentile_us,
};

use super::{fine_percentiles_us, fold_rooms, folded_steps, report_members, report_steps};

mod rules;
mod wire;

/// Steps per shard. Different on purpose: the shards really do step in
/// lockstep, but only a fixture where they do not can pin `steps` to MAX
/// and weight the two means correctly.
const STEPS: [u64; 3] = [100, 300, 200];
/// Sum of [`STEPS`] — the population the folded histograms cover.
const TOTAL_STEPS: u64 = 600;

/// Shard `i`'s report. Every field differs across the three shards, and
/// the struct literal is exhaustive: a field added to [`RoomReport`]
/// fails to compile here until the fixture gives it a value, and in
/// `fold_rooms` until the fold gives it a rule.
pub(in crate::report) fn shard(i: usize) -> RoomReport {
    let steps = STEPS[i];
    // Budget overflow mass, so `over_budget_frac` over the folded
    // histogram is a real fraction and not a rounding of one shard's.
    let over = [1u64, 2, 3][i];
    let mut step_hist = [0u64; HIST_BINS];
    step_hist[0] = steps - over;
    step_hist[HIST_OVERFLOW_BIN] = over;
    // Fine bins chosen so the p50 taken over the HISTOGRAM population
    // (600) and the p50 taken over the folded `steps` (300, the room's
    // tick count) land in DIFFERENT bins — see
    // `a_percentile_over_the_folded_histogram_needs_the_folded_population`.
    let mut step_fine_hist = [0u64; FINE_HIST_BINS];
    step_fine_hist[[2usize, 9, 1][i]] = steps;
    RoomReport {
        room: RoomId((1 << 16) + i as u64),
        steps,
        hz: [30.0, 28.0, 29.0][i],
        budget_us: 33_333,
        step_min_us: [90, 25, 60][i],
        step_mean_us: [10.0, 20.0, 30.0][i],
        step_max_us: [400, 900, 200][i],
        step_hist,
        step_fine_hist,
        late_min_us: [4_000, 700, 2_500][i],
        late_mean_us: [100.0, 200.0, 300.0][i],
        late_max_us: [5_000, 9_000, 3_000][i],
        lagged_events: [1, 2, 3][i],
        lagged_ticks: [10, 20, 30][i],
        dropped: [2, 3, 4][i],
        dropped_s: [1.5, 2.5, 3.0][i],
        sends_closed: [1, 0, 6][i],
        keepalive_resends: [5, 6, 7][i],
        snapshots: [100, 200, 300][i],
        snap_bytes_s: [1_000.0, 2_000.0, 4_000.0][i],
        snap_bytes_max: [300, 1_200, 700][i],
        snap_overflows: [1, 0, 2][i],
        snap_records: [1_000, 2_000, 3_000][i],
        shipped_bytes: [10_000, 20_000, 30_000][i],
        shipped_s: [100.0, 200.0, 300.0][i],
        shipped_frames: [40, 50, 60][i],
        private_frames: [4, 5, 6][i],
        groups: [1, 2, 3][i],
        members: [11, 13, 26][i],
        max_group: [5, 9, 4][i],
        joins: [11, 13, 26][i],
        leaves: [1, 2, 3][i],
        detached: [2, 3, 4][i],
        resumes: [1, 2, 3][i],
        resume_rejected_stale: [4, 5, 6][i],
        detach_expired_despawn: [7, 8, 9][i],
        detach_expired_ai: [1, 1, 1][i],
        detach_forced: [1, 0, 2][i],
        effects_applied: [10, 20, 40][i],
        effects_forwarded: [1, 2, 3][i],
        effects_orphaned: [0, 1, 1][i],
        effects_dropped: [2, 0, 1][i],
        effects_refused: [3, 3, 3][i],
        migrations_out: [5, 6, 7][i],
        migrations_in: [7, 6, 5][i],
        migrations_failed: [0, 0, 4][i],
        team_exports: [30, 30, 29][i],
        team_export_drops: [0, 1, 0][i],
        team_export_records: [300, 310, 320][i],
        team_over_cap: [0, 0, 2][i],
        team_imports: [60, 61, 62][i],
        team_import_records: [900, 910, 920][i],
        team_expired: [1, 0, 0][i],
        team_over_budget: [0, 3, 1][i],
        requests_local: [10, 20, 30][i],
        requests_external: [1, 2, 3][i],
        requests_rejected_malformed: [1, 0, 0][i],
        requests_rejected_dup: [0, 2, 0][i],
        requests_rejected_no_handler: [0, 0, 3][i],
        requests_rejected_logic: [4, 0, 0][i],
        requests_rejected_conn_cap: [0, 5, 0][i],
        requests_rejected_room_cap: [0, 0, 6][i],
        requests_refused_congested: [2, 0, 9][i],
        requests_dropped_unread: [0, 3, 5][i],
        requests_dropped_unbound: [1, 1, 0][i],
        actions_dropped_unread: [4, 0, 2][i],
        actions_dropped_unbound: [0, 0, 9][i],
        requests_timed_out: [7, 8, 9][i],
        requests_late: [1, 2, 4][i],
        requests_undelivered: [0, 6, 1][i],
        requests_abandoned: [2, 0, 3][i],
        pending_requests: [3, 5, 7][i],
        metrics_dropped: [2, 4, 8][i],
        logic: logic(i),
    }
}

/// The logic counters of shard `i`: a SUM every shard reports, a MAX
/// whose peak is on the MIDDLE shard, a name only the last shard has,
/// and one overflowed value on shard 0.
fn logic(i: usize) -> LogicCounters {
    let mut set = LogicCounters::new();
    set.put(&KILLS, [3, 5, 7][i]);
    set.put(&PEAK, [4, 9, 2][i]);
    if i == 2 {
        set.put(&LATE, 1);
    }
    if i == 0 {
        set.add_dropped(1);
    }
    set
}

const KILLS: LogicCounter = LogicCounter::sum("kills", "Players felled.");
const PEAK: LogicCounter = LogicCounter::max("fights_peak", "Largest fight table.");
const LATE: LogicCounter = LogicCounter::sum("late_name", "");

pub(in crate::report) fn report(rooms: Vec<RoomReport>) -> MetricReport {
    MetricReport {
        metrics_dropped: 0,
        emitted_at: Instant::now(),
        rooms,
        registry: None,
        net: NetReport {
            bytes_in: 0,
            bytes_out_room: 0,
            bytes_out_control: 0,
            bytes_out_total: 0,
            frames_in: 0,
            frames_out: 0,
            actions_dropped: 0,
            violations: 0,
            input_rate_limited: 0,
            actions_dropped_closed: 0,
            requests_dropped_closed: 0,
            requests_dropped_full: 0,
            requests_no_room: 0,
            heartbeats_throttled_preauth: 0,
            heartbeats_throttled_authed: 0,
            frames_out_closed: 0,
            close_notices_dropped: 0,
            server_closes: Default::default(),
        },
        actions_dropped_top: Vec::new(),
    }
}

fn three_shards() -> MetricReport {
    report(vec![shard(0), shard(1), shard(2)])
}

/// Floats are compared against an exactly representable expectation with
/// a tolerance, because a weighted mean divides.
#[track_caller]
fn close(got: f64, want: f64, what: &str) {
    assert!((got - want).abs() < 1e-9, "{what}: got {got}, want {want}");
}
