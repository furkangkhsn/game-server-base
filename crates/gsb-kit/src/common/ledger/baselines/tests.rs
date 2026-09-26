//! The drop half of [`Baselines`] (F11): what a dropped batch takes, and
//! the storm bound on the re-sends.

use gsb_core::id::PlayerId;

use super::{Baselines, RESEND_WAIT_MAX};

const P: PlayerId = PlayerId(7);

/// A baselined player on step 10.
fn settled() -> Baselines<u8> {
    let mut b = Baselines::default();
    assert!(b.owed(P, 0, 9, || false), "the join's one-shot full");
    assert!(!b.owed(P, 0, 10, || false));
    b
}

/// A drop of a batch with view content takes the baseline: the next
/// step owes a one-shot full again. Without view content, nothing.
#[test]
fn a_drop_with_view_content_takes_the_baseline() {
    let mut b = settled();
    b.dropped(P, 10, false);
    assert!(!b.owed(P, 0, 11, || false), "an ack-only batch: kept");
    b.dropped(P, 11, true);
    assert!(b.owed(P, 0, 12, || false), "re-sent on the next step");
    assert!(!b.owed(P, 0, 13, || false), "and baselined again");

    // A group full on the re-send step baselines it instead.
    b.dropped(P, 13, true);
    assert!(!b.owed(P, 0, 14, || true), "the group's own full");
    assert!(b.holds(P));
}

/// The storm: every batch of the player is dropped. The drop-triggered
/// fulls go out 1, 2, 4, 8, 16 steps after the 1st … 5th drop that took
/// a baseline, then every `RESEND_WAIT_MAX` steps — six in the first 63
/// steps, never two in a row after the first.
#[test]
fn a_storm_is_paced() {
    let mut b = settled();
    b.dropped(P, 10, true); // the storm begins: every batch dropped
    let mut sent = Vec::new();
    for now in 11..=200u64 {
        if b.owed(P, 0, now, || false) {
            sent.push(now - 10);
        }
        // The group frame (a delta) rides every batch: view content,
        // dropped all the same.
        b.dropped(P, now, true);
    }
    assert_eq!(sent[..6], [1, 3, 7, 15, 31, 63], "{sent:?}");
    for w in sent[5..].windows(2) {
        assert_eq!(w[1] - w[0], RESEND_WAIT_MAX, "{sent:?}");
    }
    assert_eq!(sent.len(), 6 + (190 - 63) / 32, "{sent:?}");
    assert_eq!(b.paced(), 1, "one entry, not a queue");
}

/// A keep-alive full while the re-send waits baselines the player; its
/// drop pushes the wait further (the pacing does not start over).
#[test]
fn a_dropped_keepalive_during_the_wait_escalates() {
    let mut b = settled();
    b.dropped(P, 10, true); // wait 1
    assert!(b.owed(P, 0, 11, || false));
    b.dropped(P, 11, true); // wait 2 → 13
    assert!(!b.owed(P, 0, 12, || true), "a keep-alive full");
    b.dropped(P, 12, true); // wait 4 → 16
    for now in 13..16 {
        assert!(!b.owed(P, 0, now, || false), "step {now} waits");
    }
    assert!(b.owed(P, 0, 16, || false));
}

/// Every even step's batch dropped: the pacing escalates the same way
/// (the delivered odd batches are not an all-clear; here the re-sends
/// land on even steps from the second one on, and are dropped too) —
/// and ends once the baseline stood `RESEND_WAIT_MAX` steps past the
/// last slot.
#[test]
fn alternating_drops_escalate_and_a_quiet_stretch_resets() {
    let mut b = settled();
    let mut sent = Vec::new();
    for now in 10..=80u64 {
        if b.owed(P, 0, now, || false) {
            sent.push(now);
        }
        if now % 2 == 0 {
            b.dropped(P, now, true);
        }
    }
    assert_eq!(sent, [11, 14, 18, 26, 42, 74], "{sent:?}");

    // Quiet from 81: the re-send dropped on 74 goes out on 106, the
    // pacing holds until 106 + `RESEND_WAIT_MAX`, then a drop re-sends
    // at once.
    for now in 81..106 {
        assert!(!b.owed(P, 0, now, || false), "step {now} waits");
    }
    assert!(b.owed(P, 0, 106, || false));
    for now in 107..138 {
        assert!(!b.owed(P, 0, now, || false));
    }
    assert_eq!(b.paced(), 1, "still paced at step 137");
    assert!(!b.owed(P, 0, 138, || false));
    assert_eq!(b.paced(), 0, "the pacing is over");
    b.dropped(P, 138, true);
    assert!(b.owed(P, 0, 139, || false), "at once again");
}

/// A group change while the re-send waits: still paced (the bound is
/// per player, not per group); a leave or a resume clears both tables.
#[test]
fn the_tables_stay_bounded() {
    let mut b = settled();
    b.dropped(P, 10, true);
    assert!(b.owed(P, 0, 11, || false));
    b.dropped(P, 11, true); // next slot: 13
    assert!(!b.owed(P, 1, 12, || false), "another group, same wait");
    assert!(b.owed(P, 1, 13, || false));
    b.dropped(P, 13, true);
    b.forget(P); // the player left
    assert_eq!((b.len(), b.paced()), (0, 0));
    assert!(b.owed(P, 1, 14, || false), "a new session is unpaced");

    // A drop reported for a player without a baseline takes nothing.
    let mut b: Baselines<u8> = Baselines::default();
    b.dropped(PlayerId(9), 1, true);
    assert_eq!((b.len(), b.paced()), (0, 0));
}
