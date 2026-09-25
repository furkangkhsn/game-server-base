//! The ledger under a send rate (A10; the fixture's `Rated` codec: x in
//! `[32, 48)` is every 4th step): a changed record that is not due stays
//! out of the delta with its held value NOT advanced, and goes out on
//! its due step with its CURRENT value; a record entering the view and
//! a removal go out at once; the fulls (fresh, keep-alive, one-shot) and
//! the full mode carry every current value.

use super::*;
use crate::codec::SendEvery;
use crate::testing::Rated;

const RATED: Rated<FixCodec> = Rated(FixCodec);

/// Wire 1's (every 4th step) first due step after `from + 3`, and the
/// three steps before it — none of them due. (A group is asked every
/// step: a skipped step would make it fresh.)
fn schedule(from: u64) -> (u64, [u64; 3]) {
    let due = (from + 4..)
        .find(|&s| SendEvery::Ticks4.due(s, 1))
        .expect("due");
    (due, [due - 3, due - 2, due - 1])
}

fn emit(
    ledger: &mut SetLedger<WirePos>,
    step: u64,
    now: &[(u64, i32, i32)],
) -> Option<WorldSnapshot> {
    let (mut out, mut n) = (BytesMut::new(), 0);
    match ledger.emit_delta(&RATED, step, step, &content(now), &mut out, &mut n) {
        Emitted::Silent => None,
        Emitted::Delta => Some(decode(&out)),
        Emitted::Full => panic!("an established group"),
    }
}

/// Wire 1 changes on every step between two due steps: nothing goes out
/// until the due step, which carries the value it has THEN; the ledger
/// kept the old value meanwhile (a change back to it is no change).
#[test]
fn a_change_waits_for_its_due_step_and_goes_out_current() {
    let (due, early) = schedule(1);
    let mut ledger = SetLedger::default();
    let (mut out, mut n) = (BytesMut::new(), 0);
    let first = ledger.emit_delta(
        &RATED,
        early[0] - 1,
        1,
        &content(&[(1, 40, 0)]),
        &mut out,
        &mut n,
    );
    assert_eq!(first, Emitted::Full);
    for (i, &step) in early.iter().enumerate() {
        let x = 41 + i as i32;
        assert!(
            emit(&mut ledger, step, &[(1, x, 0)]).is_none(),
            "step {step}: not due"
        );
    }
    let s = emit(&mut ledger, due, &[(1, 45, 0)]).expect("due: the change goes out");
    assert!(s.delta && s.removed.is_empty());
    assert_eq!(
        upserts(&s),
        [(1, 45, 0)].into_iter().collect(),
        "the current value"
    );
    // Held advanced to 45 on the due step: unchanged is silent after it.
    assert!(emit(&mut ledger, due + 1, &[(1, 45, 0)]).is_none());

    // A value that returns to the held one before its due step is no
    // change at all (held never advanced to the intermediate value).
    assert!(emit(&mut ledger, due + 2, &[(1, 46, 0)]).is_none());
    assert!(emit(&mut ledger, due + 3, &[(1, 45, 0)]).is_none());
    assert!(
        emit(&mut ledger, due + 4, &[(1, 45, 0)]).is_none(),
        "due, and back where the clients are"
    );
}

/// Entering the view and leaving it ignore the rate: a new record and a
/// removal go out on a step that is not due for anyone, while the
/// pending change of wire 1 stays out.
#[test]
fn entering_and_leaving_go_out_at_once() {
    let (_, early) = schedule(1);
    let mut ledger = SetLedger::default();
    let (mut out, mut n) = (BytesMut::new(), 0);
    let held = content(&[(1, 40, 0), (9, 33, 0)]);
    ledger.emit_delta(&RATED, early[0] - 1, 1, &held, &mut out, &mut n);
    let s = emit(&mut ledger, early[0], &[(1, 41, 0), (5, 34, 0)]).expect("an entry and a removal");
    assert_eq!(s.removed, [9], "wire 9 left");
    assert_eq!(
        upserts(&s),
        [(5, 34, 0)].into_iter().collect(),
        "wire 5 entered; wire 1 waits"
    );
}

/// Every FULL carries every current value, pending changes included:
/// the keep-alive resync (and the ledger follows it — the pending change
/// is no longer pending), the one-shot full frame, and the full mode.
#[test]
fn every_full_carries_the_current_values() {
    let (due, early) = schedule(1);
    let mut ledger = SetLedger::default();
    let (mut out, mut n) = (BytesMut::new(), 0);
    ledger.emit_delta(
        &RATED,
        early[0] - 1,
        1,
        &content(&[(1, 40, 0)]),
        &mut out,
        &mut n,
    );
    assert!(
        emit(&mut ledger, early[0], &[(1, 43, 0)]).is_none(),
        "pending"
    );
    let now = content(&[(1, 43, 0)]);
    let one_shot = ledger.full_frame(&RATED, early[0], early[0], &now, &mut n);
    assert_eq!(
        upserts(&decode(&one_shot)),
        [(1, 43, 0)].into_iter().collect()
    );
    let keep = ledger.resync(&RATED, early[0], early[0], &now, &mut n);
    assert_eq!(upserts(&decode(&keep)), [(1, 43, 0)].into_iter().collect());
    for step in [early[1], early[2], due] {
        let silent = emit(&mut ledger, step, &[(1, 43, 0)]).is_none();
        assert!(silent, "step {step}: the keep-alive re-synced it");
    }

    let mut full_mode: SetLedger<WirePos> = SetLedger::default();
    out.clear();
    assert!(full_mode.emit_full(&RATED, 1, &content(&[(1, 40, 0)]), &mut out, &mut n));
    out.clear();
    assert!(
        full_mode.emit_full(&RATED, early[0], &now, &mut out, &mut n),
        "the full mode ignores the rate"
    );
    assert_eq!(upserts(&decode(&out)), [(1, 43, 0)].into_iter().collect());
}

/// The default class changes nothing: the fixture codec and `Rated`
/// on a record of the every-step band write the same frames.
#[test]
fn the_every_step_class_is_the_default_frame() {
    for step in 2..10 {
        let mut a = established(&[(1, 5, 0), (2, 6, 0)]);
        let mut b = established(&[(1, 5, 0), (2, 6, 0)]);
        let now = content(&[(1, 7, 0), (3, 8, 0)]);
        let (mut oa, mut ob, mut n) = (BytesMut::new(), BytesMut::new(), 0);
        let ea = a.emit_delta(&FixCodec, step, step, &now, &mut oa, &mut n);
        let eb = b.emit_delta(&RATED, step, step, &now, &mut ob, &mut n);
        assert_eq!((ea, decode(&oa)), (eb, decode(&ob)), "step {step}");
    }
}
