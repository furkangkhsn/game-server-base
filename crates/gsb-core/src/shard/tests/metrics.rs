//! The shard's metrics sample: the step-duration histograms.

use super::*;
use crate::metrics::fine_hist_percentile_us;

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
    // Deliberately NOT bracketed from below by `step_min_us`: that field
    // is not a minimum. Both actors set it only on `steps == 1` and never
    // lower it afterwards, so it holds the (typically cold, slowest) first
    // step's duration forever — a defect of its own, shared by the room
    // and the shard, and out of this change's scope.
    assert!(
        p50 <= s.step_max_us,
        "the fine histogram must bin the duration the step measured: p50 \
         {p50} µs exceeds step_max_us {}",
        s.step_max_us
    );
}
