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

/// The core's side of a batch, as the fan-out reports it: `dropped`
/// with view content, or delivered — the first delivered after a run of
/// drops is the resume.
fn fan_out(b: &mut Baselines<u8>, now: u64, dropped: bool, dropping: &mut bool) {
    if dropped {
        b.dropped(P, now, true);
    } else if *dropping {
        b.resumed(P);
    }
    *dropping = dropped;
}

/// A long stall, then the channel drains: the first batch that gets
/// through releases the paced re-send — the full rides the very next
/// frame instead of waiting out the back-off.
#[test]
fn the_first_delivered_batch_releases_the_wait() {
    let mut b = settled();
    let mut dropping = false;
    let mut sent = Vec::new();
    for now in 10..=110u64 {
        if b.owed(P, 0, now, || false) {
            sent.push(now);
        }
        fan_out(&mut b, now, now <= 100, &mut dropping);
    }
    assert_eq!(sent, [11, 13, 17, 25, 41, 73, 102], "{sent:?}");
    assert!(b.holds(P));
}

/// Every even step's batch dropped: each delivered batch releases at
/// most one re-send (here the re-sends land on the dropped even steps
/// from the second on) — never more than one per batch that gets
/// through; the pacing ends once the baseline stood `RESEND_WAIT_MAX`
/// steps past the last slot.
#[test]
fn alternating_drops_release_one_resend_per_delivered_batch() {
    let mut b = settled();
    let mut dropping = false;
    let mut sent = Vec::new();
    for now in 10..=30u64 {
        if b.owed(P, 0, now, || false) {
            sent.push(now);
        }
        fan_out(&mut b, now, now % 2 == 0, &mut dropping);
    }
    assert_eq!(sent, [11, 14, 16, 18, 20, 22, 24, 26, 28, 30], "{sent:?}");

    // Quiet from 31: the full of 30 was dropped (its slot: 62), the
    // delivered 31 releases it for 32; the pacing ends at 62 + 32.
    for now in 31..=100u64 {
        if b.owed(P, 0, now, || false) {
            sent.push(now);
        }
        fan_out(&mut b, now, false, &mut dropping);
        assert_eq!(b.paced(), usize::from(now < 94), "step {now}");
    }
    assert_eq!(sent.last(), Some(&32));
    b.dropped(P, 100, true);
    assert!(b.owed(P, 0, 101, || false), "at once again");
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
