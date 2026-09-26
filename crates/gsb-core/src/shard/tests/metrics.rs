//! The shard's metrics sample: the step-duration histograms.

use super::*;
use crate::metrics::fine_hist_percentile_us;

mod members;

/// The shard's sample ships `step_fine_hist`, so the shard's step path
/// must FILL it.
///
/// It did not. The coarse `step_hist` and every `step_*`/`late_*` scalar
/// were measured on the shard exactly as on the room, but the fine
/// fixed-bin histogram — the one the sub-budget percentiles are taken
/// from — was only ever incremented by the room actor. The shard sampled
/// its own permanently-zero copy and shipped it.
///
/// That is not a merely-missing number: `fine_hist_percentile_us` answers
/// `None` for an empty histogram, and the Prometheus surface renders the
/// `gsb_room_step_duration_us` p50/p99 lines behind an `if let Some`. So
/// a sharded room emitted NO quantile lines at all, while the loadgen's
/// summary, which falls back with `unwrap_or(FINE_HIST_CAP_US)`, reported
/// every sharded room as pinned at the cap. One consumer said nothing and
/// the other said "slow"; neither said anything true.
///
/// Locked on both halves: the histogram counts, and the percentile the
/// renderer gates on resolves.
#[tokio::test]
async fn sharded_step_fills_the_fine_duration_histogram() {
    let mut a = bare_shard(0);
    for t in 1..=5 {
        assert!(a.step(&tinfo(t)), "the shard keeps running");
    }

    let s = a.sample();
    assert_eq!(s.steps, 5, "five steps were run");
    assert_eq!(
        s.step_hist.iter().sum::<u64>(),
        s.steps,
        "the coarse histogram counts every step (it always did)"
    );

    // Before the fix this sum was structurally zero, forever. Every step
    // of an empty-world rig is orders of magnitude under the fine cap
    // (`FINE_HIST_CAP_US` = 4096 µs), so all five must be present — the
    // same shape as the room-side collector test's coarse assertion.
    let fine: [u64; crate::metrics::FINE_HIST_BINS] = s.step_fine_hist.map(u64::from);
    assert_eq!(
        fine.iter().sum::<u64>(),
        s.steps,
        "the shard's step path must increment the fine histogram once per \
         step, the way the room actor's does"
    );

    let p50 = fine_hist_percentile_us(&fine, s.steps, 50).expect(
        "the p50 the Prometheus surface gates gsb_room_step_duration_us on \
         must resolve for a sharded room",
    );
    // The answer is the lower edge of a bin a REAL step landed in, so it
    // cannot exceed the largest step this rig measured. Exact, not
    // timing-dependent: it separates "binned the measured duration" from
    // "binned some other number".
    //
    // It is now bracketed from BELOW by `step_min_us` as well. That was
    // impossible when this test was written: `step_min_us` was not a
    // minimum on either actor (assigned only under `steps == 1`, never
    // lowered), so it carried the cold first step's duration and sat
    // ABOVE a warm p50 — the bracket would have failed on correct code,
    // and the comment here said so. The "min counters" round made it a
    // real minimum, so the bracket became a true statement about the same
    // five observations: the assertion gets strictly stronger.
    //
    // Compared at BIN granularity, because `fine_hist_percentile_us`
    // answers the bin's LOWER EDGE (the documented ±8 µs semantics): a
    // p50 in the same bin as the minimum reads up to 7 µs below it, which
    // is the helper working, not the ranking being wrong.
    let min_bin = crate::metrics::fine_hist_index(s.step_min_us)
        .expect("this rig's steps are far under the fine cap");
    let p50_bin = (p50 / crate::metrics::FINE_HIST_US_PER_BIN) as usize;
    assert!(
        p50_bin >= min_bin,
        "the fine histogram must bin the duration the step measured: the \
         p50 bin ({p50_bin}, edge {p50} µs) is below the bin the minimum \
         {} µs lands in ({min_bin})",
        s.step_min_us
    );
    assert!(
        p50 <= s.step_max_us,
        "the fine histogram must bin the duration the step measured: p50 \
         {p50} µs exceeds step_max_us {}",
        s.step_max_us
    );
}

/// `late_min_us` must be a MINIMUM — a later, smaller tick latency has to
/// pull it down.
///
/// It did not. Both actors seeded the pair on `steps == 1` and then only
/// ever raised the maximum; no arm lowered the minimum, so the field held
/// the FIRST step's latency for the process's whole life and every
/// consumer that reads it — the `gsb-metric scope=room` line, the
/// `gsb_room_late_min_us` gauge whose HELP says "Minimum", the loadgen
/// summary — printed a first-observation under a minimum's name.
///
/// Deterministic, not timing-dependent: the latency is `step start −
/// t.at`, and the test owns `t.at`. A tick stamped 50 ms in the past is
/// observed at ≥ 50 ms late; the next, stamped 2 ms in the past, at ≥ 2 ms
/// — so the second observation is strictly the smaller one by a margin
/// that no scheduling jitter in a bare, empty-world step can close.
#[tokio::test]
async fn a_faster_tick_lowers_the_shards_late_minimum() {
    let mut a = bare_shard(0);

    assert!(a.step(&tinfo_late_by(1, Duration::from_millis(50))));
    let seeded = a.sample().late_min_us;
    assert!(
        seeded >= 50_000,
        "the first step must observe the 50 ms the tick was stamped late: {seeded} µs"
    );

    assert!(a.step(&tinfo_late_by(2, Duration::from_millis(2))));

    let s = a.sample();
    assert_eq!(s.steps, 2, "two steps were run");
    // The second observation was at most ~a few ms; the first was at
    // least 50 ms. A true minimum tracks the second.
    assert!(
        s.late_min_us < seeded,
        "a later, smaller tick latency must lower the reported minimum: \
         late_min_us {} still equals the first step's {seeded} µs",
        s.late_min_us
    );
    assert!(
        s.late_min_us < 20_000,
        "late_min_us {} µs must track the ~2 ms observation, not the 50 ms one",
        s.late_min_us
    );
    // The seeding half of the contract: a minimum initialised to 0 could
    // never be lowered and would report 0 forever, so it must start from
    // a real observation and stay above zero here (both ticks were
    // stamped milliseconds in the past).
    assert!(
        s.late_min_us > 0,
        "late_min_us must be seeded from the first observation, never left at 0"
    );
    assert!(
        s.late_max_us >= seeded,
        "the maximum must still hold the 50 ms observation: {} µs",
        s.late_max_us
    );
    assert!(
        s.late_min_us <= s.late_max_us,
        "min {} must not exceed max {}",
        s.late_min_us,
        s.late_max_us
    );
}
