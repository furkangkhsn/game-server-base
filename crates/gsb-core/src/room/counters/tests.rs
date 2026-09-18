//! The step/latency accounting both actors share: exact-value locks on
//! synthetic durations, so the `min` arm is pinned without depending on
//! how long a real step happens to take.

use super::*;
use crate::metrics::{FINE_HIST_US_PER_BIN, fine_hist_index, hist_index};

/// 30 Hz, the tick budget the rooms in this repo are measured at.
const BUDGET_US: u64 = 33_333;

/// `observe_step_us` must keep a MINIMUM: a later, smaller duration
/// lowers it, a later, larger one does not raise it.
///
/// This is the exact-value half of the behaviour lock. Before the fix
/// both actors assigned `step_min_us` only under `steps == 1` and no arm
/// ever lowered it, so the sequence below reported 900 — the first and
/// LARGEST duration — as the minimum.
#[test]
fn step_minimum_tracks_the_smallest_observation() {
    let mut m = RoomCounters::default();
    // Descending then ascending, so neither "keep the first" nor "keep
    // the last" can pass: the smallest is in the middle.
    for (step, us) in [(1u64, 900u64), (2, 120), (3, 40), (4, 310), (5, 1_500)] {
        m.observe_step_us(step, BUDGET_US, us);
    }

    assert_eq!(m.step_min_us, 40, "the minimum is the smallest observation");
    assert_eq!(m.step_max_us, 1_500, "the maximum is the largest");
    assert_eq!(
        m.step_sum_us,
        900 + 120 + 40 + 310 + 1_500,
        "the sum is exact"
    );
    assert_eq!(
        m.step_hist.iter().sum::<u64>(),
        5,
        "every step is binned exactly once in the log2 histogram"
    );
    assert_eq!(
        m.step_fine_hist.iter().copied().map(u64::from).sum::<u64>(),
        5,
        "every sub-cap step is binned exactly once in the fine histogram"
    );
}

/// Same contract for the tick-latency pair.
#[test]
fn late_minimum_tracks_the_smallest_observation() {
    let mut m = RoomCounters::default();
    for (step, us) in [(1u64, 5_000u64), (2, 800), (3, 120), (4, 9_000)] {
        m.observe_late_us(step, us);
    }

    assert_eq!(
        m.late_min_us, 120,
        "the minimum is the smallest observation"
    );
    assert_eq!(m.late_max_us, 9_000, "the maximum is the largest");
    assert_eq!(m.late_sum_us, 5_000 + 800 + 120 + 9_000, "the sum is exact");
}

/// The seeding rule, stated as its own lock: the first observation SETS
/// both ends instead of being compared against the zero-initialised
/// fields.
///
/// This is the trap a naive "make it a minimum" fix falls into. Dropping
/// the seeding branch and writing `min = min.min(x)` from a zero start
/// leaves the minimum at 0 forever — still not a minimum, and now
/// silently so, because 0 is a plausible-looking duration rather than an
/// obviously cold first step.
#[test]
fn the_first_observation_seeds_both_ends_so_a_minimum_is_never_stuck_at_zero() {
    let mut m = RoomCounters::default();
    assert_eq!(m.step_min_us, 0, "the counter starts zero-initialised");
    assert_eq!(m.late_min_us, 0, "the counter starts zero-initialised");

    m.observe_step_us(1, BUDGET_US, 250);
    m.observe_late_us(1, 700);

    assert_eq!(m.step_min_us, 250, "the first step SEEDS the minimum");
    assert_eq!(m.step_max_us, 250, "and the maximum");
    assert_eq!(
        m.late_min_us, 700,
        "the first step SEEDS the latency minimum"
    );
    assert_eq!(m.late_max_us, 700, "and the maximum");

    // Every later observation is larger, so a correct minimum stays at
    // the seed — and a minimum that had been left at 0 would still read 0
    // here, which is what this asserts against.
    for step in 2..=50u64 {
        m.observe_step_us(step, BUDGET_US, 250 + step);
        m.observe_late_us(step, 700 + step);
    }
    assert_eq!(m.step_min_us, 250, "no later step was faster");
    assert_eq!(m.late_min_us, 700, "no later tick was less late");
    assert!(
        m.step_min_us > 0 && m.late_min_us > 0,
        "never stuck at zero"
    );
}

/// The extremes and the histograms must describe the SAME observations:
/// the minimum has to land in the lowest occupied fine bin and the
/// maximum in the highest, so a fix that moved one and not the other
/// cannot pass.
#[test]
fn the_extremes_agree_with_the_histograms() {
    let mut m = RoomCounters::default();
    let samples = [900u64, 120, 40, 310, 1_500];
    for (i, us) in samples.iter().enumerate() {
        m.observe_step_us(i as u64 + 1, BUDGET_US, *us);
    }

    let lowest = m
        .step_fine_hist
        .iter()
        .position(|&n| n > 0)
        .expect("all five samples are under the fine cap");
    let highest = m
        .step_fine_hist
        .iter()
        .rposition(|&n| n > 0)
        .expect("all five samples are under the fine cap");
    assert_eq!(
        Some(lowest),
        fine_hist_index(m.step_min_us),
        "the minimum must sit in the lowest occupied fine bin"
    );
    assert_eq!(
        Some(highest),
        fine_hist_index(m.step_max_us),
        "the maximum must sit in the highest occupied fine bin"
    );
    assert!(
        m.step_min_us < (lowest as u64 + 1) * FINE_HIST_US_PER_BIN,
        "the minimum is inside its bin, not past its upper edge"
    );
    assert!(
        m.step_hist[hist_index(BUDGET_US, m.step_max_us)] > 0,
        "the maximum's log2 bin is occupied"
    );
}

/// A step at or above the fine cap stays out of the fine histogram (the
/// documented overflow rule) while still moving the scalars — the
/// pre-existing behaviour, restated here because the accounting moved
/// into one function and must not have changed it.
#[test]
fn a_step_over_the_fine_cap_still_moves_the_scalars() {
    let mut m = RoomCounters::default();
    m.observe_step_us(1, BUDGET_US, 40);
    m.observe_step_us(2, BUDGET_US, crate::metrics::FINE_HIST_CAP_US + 10);

    assert_eq!(m.step_min_us, 40, "the sub-cap step is still the minimum");
    assert_eq!(
        m.step_max_us,
        crate::metrics::FINE_HIST_CAP_US + 10,
        "the over-cap step is the maximum"
    );
    assert_eq!(
        m.step_hist.iter().sum::<u64>(),
        2,
        "the log2 histogram counts both steps"
    );
    assert_eq!(
        m.step_fine_hist.iter().copied().map(u64::from).sum::<u64>(),
        1,
        "the fine histogram counts only the sub-cap step"
    );
}
