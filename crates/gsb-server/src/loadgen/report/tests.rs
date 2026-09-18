//! The sharded-room fold: what happens to a minimum when N shard
//! reports become one room report.

use std::time::Instant;

use gsb_core::id::RoomId;
use gsb_core::metrics::{FINE_HIST_BINS, HIST_BINS, MetricReport, NetReport, RoomReport};

use super::fold_rooms;

/// One shard's report, everything but the fields a test varies at zero.
fn shard_report(room: u64, steps: u64, step_min_us: u64, late_min_us: u64) -> RoomReport {
    RoomReport {
        room: RoomId(room),
        steps,
        hz: 30.0,
        budget_us: 33_333,
        step_min_us,
        step_mean_us: 100.0,
        step_max_us: 400,
        step_hist: [0; HIST_BINS],
        step_fine_hist: [0; FINE_HIST_BINS],
        late_min_us,
        late_mean_us: 500.0,
        late_max_us: 900,
        lagged_events: 0,
        lagged_ticks: 0,
        dropped: 0,
        dropped_s: 0.0,
        keepalive_resends: 0,
        snapshots: 0,
        snap_bytes_s: 0.0,
        snap_bytes_max: 0,
        snap_overflows: 0,
        snap_records: 0,
        shipped_bytes: 0,
        shipped_s: 0.0,
        groups: 1,
        members: 1,
        max_group: 1,
        joins: 0,
        leaves: 0,
        detached: 0,
        resumes: 0,
        resume_rejected_stale: 0,
        detach_expired_despawn: 0,
        detach_expired_ai: 0,
        requests_local: 0,
        requests_external: 0,
        requests_rejected_malformed: 0,
        requests_rejected_dup: 0,
        requests_rejected_no_handler: 0,
        requests_rejected_logic: 0,
        requests_rejected_conn_cap: 0,
        requests_rejected_room_cap: 0,
        requests_timed_out: 0,
        requests_late: 0,
        pending_requests: 0,
        metrics_dropped: 0,
    }
}

fn report(rooms: Vec<RoomReport>) -> MetricReport {
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
        },
        actions_dropped_top: Vec::new(),
    }
}

/// Folding N shard reports into one room report must fold a MINIMUM with
/// `min` — over every minimum the report carries.
///
/// `step_min_us` was folded correctly; `late_min_us` was not folded at
/// all. The accumulator starts from `*first` and the loop never touched
/// `late_min_us`, so a sharded room's reported "minimum tick latency" was
/// whichever shard happened to sort first in the report — not the room's
/// minimum, and not even a deterministic shard's. The shards are ordered
/// by their sample id (`room << 16 | index`), so it was always shard 0's:
/// a second, quieter instance of exactly the bug the actors had, one
/// layer up.
///
/// The fixture puts the smaller value on the SECOND shard for both
/// fields, so a fold that keeps the first element fails and one that
/// keeps the last passes only by accident — hence the third shard,
/// which restores a larger value after the smaller one.
#[test]
fn folding_shards_takes_the_minimum_of_every_minimum() {
    let r = report(vec![
        shard_report(1 << 16, 100, 90, 4_000),
        shard_report((1 << 16) + 1, 100, 25, 700),
        shard_report((1 << 16) + 2, 100, 60, 2_500),
    ]);

    let folded = fold_rooms(&r).expect("three shard reports fold");

    assert_eq!(
        folded.step_min_us, 25,
        "the room's step minimum is the smallest shard's, not the first shard's"
    );
    assert_eq!(
        folded.late_min_us, 700,
        "the room's tick-latency minimum is the smallest shard's, not the \
         first shard's (this was unfolded: it reported shard 0's 4000 µs)"
    );
    // The maxima are the counterweight: the same fold must still take the
    // largest, so a copy-paste that turned `max` into `min` is caught here.
    assert_eq!(folded.step_max_us, 400, "step maximum across shards");
    assert_eq!(folded.late_max_us, 900, "late maximum across shards");
    assert!(
        folded.late_min_us <= folded.late_max_us,
        "min {} must not exceed max {}",
        folded.late_min_us,
        folded.late_max_us
    );
}

/// A single-room (non-sharded) report is the identity fold: the early
/// return hands back the one element untouched, minima included.
#[test]
fn folding_one_room_is_the_identity() {
    let only = shard_report(7, 50, 33, 1_234);
    let folded = fold_rooms(&report(vec![only])).expect("one room folds");
    assert_eq!(folded.step_min_us, 33);
    assert_eq!(folded.late_min_us, 1_234);
}
