//! `SnapshotBudget`'s rules with synthetic budgets: full rate when the
//! frame fits, the credit's rate when it does not, the staleness bound
//! (counted), no burst after a gap, and a table that forgets.

use super::*;

const P: PlayerId = PlayerId(1);

/// Offer `n` ticks (from tick 1) of frames of `bytes` at `budget`;
/// the shipped pattern.
fn run(b: &mut SnapshotBudget, n: u64, bytes: usize, budget: usize) -> Vec<bool> {
    (1..=n).map(|t| b.admit(P, t, bytes, budget)).collect()
}

#[test]
fn a_frame_that_fits_ships_every_tick() {
    let mut b = SnapshotBudget::new();
    assert!(run(&mut b, 40, 1_000, 1_000).iter().all(|s| *s));
    assert!(run(&mut b, 40, 400, 1_000).iter().all(|s| *s));
    assert_eq!(b.forced(), 0);
}

#[test]
fn a_frame_three_budgets_long_ships_every_third_tick() {
    let mut b = SnapshotBudget::new();
    let got = run(&mut b, 12, 3_000, 1_000);
    let want: Vec<bool> = (1..=12).map(|t| t % 3 == 0).collect();
    assert_eq!(got, want);
    // Over a long run the bytes shipped never outrun the budget.
    let mut b = SnapshotBudget::new();
    let shipped = run(&mut b, 3_000, 2_500, 1_000)
        .iter()
        .filter(|s| **s)
        .count();
    assert_eq!(
        shipped, 1_200,
        "2.5 budgets per frame: 2 frames per 5 ticks"
    );
    assert_eq!(b.forced(), 0);
}

#[test]
fn varying_frames_spend_the_same_credit() {
    let mut b = SnapshotBudget::new();
    // 1 000 B/tick: a 1 500 B frame on tick 1 waits; tick 2 has 2 000,
    // ships, leaves 500; a 400 B frame on tick 3 (1 500) ships.
    assert!(!b.admit(P, 1, 1_500, 1_000));
    assert!(b.admit(P, 2, 1_500, 1_000));
    assert!(b.admit(P, 3, 400, 1_000));
    assert!(b.admit(P, 4, 2_000, 1_000), "1 100 + 1 000 covers 2 000");
}

#[test]
fn the_staleness_bound_ships_one_frame_in_sixteen_and_counts_it() {
    let mut b = SnapshotBudget::new();
    let got = run(&mut b, 48, 3_000, 0);
    let shipped: Vec<u64> = (1..=48).filter(|t| got[*t as usize - 1]).collect();
    assert_eq!(shipped, vec![16, 32, 48]);
    assert_eq!(b.forced(), 3);
    let mut out = LogicCounters::new();
    b.counters(&mut out);
    assert_eq!(out.slots().len(), 1, "one counter");
    assert_eq!(out.get("snapshot_budget_forced"), Some(3));
    // A tighter bound, by the room's choice.
    let mut b = SnapshotBudget::with_held_max(1);
    assert_eq!(run(&mut b, 4, 3_000, 0), vec![false, true, false, true]);
    // 0: never withhold.
    let mut b = SnapshotBudget::with_held_max(0);
    assert!(run(&mut b, 4, 3_000, 0).iter().all(|s| *s));
}

#[test]
fn a_gap_adds_its_steps_but_never_more_than_one_frame() {
    let mut b = SnapshotBudget::new();
    // Steps of one tick (the stride is learnt from two offers).
    assert!(!b.admit(P, 1, 6_000, 1_000));
    assert!(!b.admit(P, 2, 6_000, 1_000));
    // Not asked for ticks 3..=9 (its group had no frame): tick 10's
    // offer finds the credit of every step between — the path drained
    // all along — capped at one frame plus one step.
    assert!(b.admit(P, 10, 6_000, 1_000), "8 steps of budget, capped");
    assert!(!b.admit(P, 11, 6_000, 1_000), "no burst: the cap spent it");
    for t in 12..=14 {
        assert!(!b.admit(P, t, 6_000, 1_000));
    }
    assert!(b.admit(P, 15, 6_000, 1_000), "then the budget's rate");
}

/// A room stepping every other global tick is credited per STEP, not
/// per tick: the same pattern as a room stepping every tick.
#[test]
fn a_slower_room_is_credited_per_step() {
    let mut b = SnapshotBudget::new();
    let got: Vec<bool> = (1..=12).map(|s| b.admit(P, 2 * s, 3_000, 1_000)).collect();
    let want: Vec<bool> = (1..=12).map(|s| s % 3 == 0).collect();
    assert_eq!(got, want);
}

/// A frame the staleness bound sends spends all the credit there was:
/// the next one waits for the whole frame again.
#[test]
fn a_forced_frame_spends_the_credit() {
    let mut b = SnapshotBudget::new();
    let got = run(&mut b, 40, 3_000, 100);
    let shipped: Vec<u64> = (1..=40).filter(|t| got[*t as usize - 1]).collect();
    assert_eq!(
        shipped,
        vec![16, 32],
        "1 600 B of credit at 16 is spent, not kept"
    );
    assert_eq!(b.forced(), 2);
}

#[test]
fn members_are_independent_and_the_table_forgets_the_unasked() {
    let mut b = SnapshotBudget::new();
    let q = PlayerId(2);
    assert!(b.admit(P, 1, 100, 1_000));
    assert!(!b.admit(q, 1, 3_000, 1_000), "Q's credit is its own");
    assert_eq!(b.tracked(), 2);
    // Only P is asked afterwards: Q goes at the first sweep more than
    // SWEEP_TICKS after its last offer (sweeps run every SWEEP_TICKS).
    for t in 2..=(2 * SWEEP_TICKS) {
        b.admit(P, t, 100, 1_000);
    }
    assert_eq!(b.tracked(), 1);
    // Asked again, Q starts with no credit: a 1 000 B tick does not
    // cover 3 000 B even though Q once had credit.
    assert!(!b.admit(q, 2 * SWEEP_TICKS + 1, 3_000, 1_000));
    assert!(!b.admit(q, 2 * SWEEP_TICKS + 2, 3_000, 1_000));
    assert!(b.admit(q, 2 * SWEEP_TICKS + 3, 3_000, 1_000));
}
