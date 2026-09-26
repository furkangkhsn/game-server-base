//! The book's pending-change hygiene, at the primitive level (BACKLOG
//! F13): every primitive that sends a record's current value at once —
//! an appearance, a crossing, an exit — settles its pending change, so
//! the due step releases nothing for it.
//!
//! The appearance case is the one no room-level test reaches cheaply:
//! it needs a record that is PENDING in a cell and then appears there
//! again without leaving it first. The sharded × spatial composite does
//! exactly that when an entity migrates in over its own lent copy: the
//! copy's in-cell move was deferred on the strip, the arrival is placed
//! by the dirty pass as an appearance in the same cell, and the copy's
//! exit is skipped (the own record now holds the cell). Without the
//! settle, the due step re-sends the value the appearance already
//! carried — an idempotent surplus upsert on the wire.

use super::*;

const WIRE: u64 = 7;
const CELL: u8 = 1;
const EVERY: SendEvery = SendEvery::Ticks16;

/// Close the tick and return the upserts the cell's delta carries.
fn roll_updates(book: &mut CellBook<i32, u8>) -> Vec<(u64, i32)> {
    book.roll();
    book.cell_changes
        .get(&CELL)
        .map(|ch| ch.updates.clone())
        .unwrap_or_default()
}

#[test]
fn a_reappearance_settles_the_pending_change() {
    let mut book = CellBook::<i32, u8>::default();
    book.begin_tick();
    book.record_appearance(WIRE, 0, CELL, false);
    assert_eq!(roll_updates(&mut book), vec![(WIRE, 0)]);

    // A step on which the record is not due: its in-cell change waits.
    loop {
        book.begin_tick();
        if !EVERY.due(book.step, WIRE) {
            break;
        }
        assert!(roll_updates(&mut book).is_empty());
    }
    book.record_change(CELL, WIRE, 1, EVERY);
    assert!(book.deferred.contains_key(&WIRE), "the change is pending");
    assert!(roll_updates(&mut book).is_empty(), "nothing sent yet");

    // The record appears in the same cell again: its current value goes
    // out at once — and that settles the pending change.
    book.begin_tick();
    book.record_appearance(WIRE, 2, CELL, false);
    assert!(!book.deferred.contains_key(&WIRE), "nothing is pending");
    assert_eq!(roll_updates(&mut book), vec![(WIRE, 2)]);

    // Two full periods: the due step comes and goes, and releases
    // nothing (the value is already on the wire).
    for _ in 0..2 * EVERY.ticks() {
        book.begin_tick();
        assert_eq!(
            roll_updates(&mut book),
            vec![],
            "no surplus upsert at step {}",
            book.step
        );
    }
}
