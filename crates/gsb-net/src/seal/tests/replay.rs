//! The replay window alone: exact edges, bitmap wrap, far jumps, and a
//! seeded comparison against a naive set model.

use std::collections::BTreeSet;

use super::super::replay::{Check, ReplayWindow};
use super::*;

const W: u64 = REPLAY_WINDOW;

fn marked(counters: impl IntoIterator<Item = u64>) -> ReplayWindow {
    let mut w = ReplayWindow::new();
    for c in counters {
        assert_eq!(w.check(c), Check::Fresh, "counter {c}");
        w.mark(c);
    }
    w
}

#[test]
fn empty_window_takes_any_counter_then_refuses_its_duplicate() {
    let w = ReplayWindow::new();
    assert_eq!(w.check(0), Check::Fresh);
    assert_eq!(w.check(u64::MAX / 2), Check::Fresh);
    let w = marked([7]);
    assert_eq!(w.check(7), Check::Replayed);
    assert_eq!(w.check(6), Check::Fresh);
    assert_eq!(w.check(8), Check::Fresh);
}

#[test]
fn exact_lower_edge_of_the_window() {
    let top = 5000;
    let w = marked([top]);
    assert_eq!(
        w.check(top - (W - 1)),
        Check::Fresh,
        "oldest counter still inside"
    );
    assert_eq!(w.check(top - W), Check::TooOld, "first counter outside");
    let w = marked([top, top - (W - 1)]);
    assert_eq!(w.check(top - (W - 1)), Check::Replayed);
}

#[test]
fn out_of_order_inside_the_window_is_accepted_once() {
    let mut w = marked([100, 90, 99, 1, 50]);
    for c in [100, 90, 99, 1, 50] {
        assert_eq!(w.check(c), Check::Replayed);
    }
    assert_eq!(w.check(2), Check::Fresh);
    w.mark(2);
    assert_eq!(w.check(2), Check::Replayed);
}

#[test]
fn bitmap_wrap_forgets_counters_that_left_the_window() {
    // Fill a whole window, then advance: slot i now belongs to i + W.
    let mut w = marked(0..W);
    assert_eq!(w.check(W - 1), Check::Replayed);
    w.mark(W);
    assert_eq!(w.check(0), Check::TooOld);
    assert_eq!(w.check(1), Check::Replayed, "still inside, still seen");
    // A jump inside the window: slots of W+1..W+9 held 1..9 (seen) and
    // must now read unseen.
    w.mark(W + 10);
    for c in W + 1..W + 10 {
        assert_eq!(
            w.check(c),
            Check::Fresh,
            "counter {c} shares a slot with {}",
            c - W
        );
    }
    assert_eq!(w.check(10), Check::TooOld);
    assert_eq!(w.check(11), Check::Replayed);
}

#[test]
fn far_future_jump_clears_everything() {
    for jump in [W - 1, W, W + 1, 10 * W + 3] {
        let mut w = marked(0..200);
        let top = 199 + jump;
        w.mark(top);
        assert_eq!(w.check(top), Check::Replayed);
        for c in (top + 1).saturating_sub(W)..top {
            let expect = if c < 200 {
                Check::Replayed
            } else {
                Check::Fresh
            };
            assert_eq!(w.check(c), expect, "jump {jump}, counter {c}");
        }
        assert_eq!(w.check(top - W), Check::TooOld, "jump {jump}");
    }
}

#[test]
fn seeded_walk_matches_a_naive_set() {
    let mut rng = Rng(0x5eed_0001);
    let mut w = ReplayWindow::new();
    let mut seen = BTreeSet::new();
    let mut top: Option<u64> = None;
    let mut base = 0u64;
    for _ in 0..20_000 {
        // Mostly near the top, sometimes behind it, rarely a far jump.
        let c = match rng.below(20) {
            0 => base + W * (1 + rng.below(3)) + rng.below(W),
            1..=6 => base.saturating_sub(rng.below(W + 64)),
            _ => base + rng.below(64),
        };
        let expect = match top {
            Some(t) if c <= t && t - c >= W => Check::TooOld,
            _ if seen.contains(&c) => Check::Replayed,
            _ => Check::Fresh,
        };
        assert_eq!(w.check(c), expect, "counter {c}, top {top:?}");
        if expect == Check::Fresh {
            w.mark(c);
            seen.insert(c);
            top = Some(top.map_or(c, |t| t.max(c)));
            base = base.max(c);
        }
    }
}
