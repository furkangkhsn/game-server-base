//! The set-content ledger on hand-built contents (the fixture codec):
//! a delta is exactly `removed` + upserts in that wire order, silence
//! writes no bytes, freshness follows the step sequence, the full frame
//! is encoded once per step, and the full mode is the full-only rooms'
//! frame.

use std::collections::{BTreeSet, HashMap};

use bytes::BytesMut;
use prost::Message;

use super::*;
use crate::common::put_entity_records;
use crate::testing::{FixCodec, WirePos, WorldSnapshot};

fn content(records: &[(u64, i32, i32)]) -> HashMap<u64, WirePos> {
    records
        .iter()
        .map(|&(id, x, y)| (id, WirePos { x, y }))
        .collect()
}

fn decode(out: &[u8]) -> WorldSnapshot {
    WorldSnapshot::decode(out).expect("a snapshot")
}

fn upserts(s: &WorldSnapshot) -> BTreeSet<(u64, i32, i32)> {
    s.entities.iter().map(|r| (r.entity, r.x, r.y)).collect()
}

/// A ledger whose group is established at step 1 holding `held`.
fn established(held: &[(u64, i32, i32)]) -> SetLedger<WirePos> {
    let mut ledger = SetLedger::default();
    let (mut out, mut n) = (BytesMut::new(), 0);
    let first = ledger.emit_delta(&FixCodec, 1, 1, &content(held), &mut out, &mut n);
    assert_eq!(first, Emitted::Full, "a fresh group's frame is a full");
    ledger
}

/// Removals are exactly the ids that left, upserts exactly the new ids
/// and the changed values (an unchanged record is not re-sent), and the
/// `removed` entries come before every record on the wire.
#[test]
fn a_delta_is_exactly_removed_then_upserts() {
    let mut ledger = established(&[(1, 0, 0), (2, 1, 1), (3, 2, 2)]);
    let now = content(&[(2, 1, 1), (3, 5, 5), (4, 9, 9)]);
    let (mut out, mut n) = (BytesMut::new(), 0);
    assert_eq!(
        ledger.emit_delta(&FixCodec, 2, 7, &now, &mut out, &mut n),
        Emitted::Delta
    );
    let s = decode(&out);
    assert!(s.delta && s.sequence == 7 && s.cell_exits.is_empty());
    assert_eq!(s.removed, [1], "exactly the id that left");
    assert_eq!(
        upserts(&s),
        [(3, 5, 5), (4, 9, 9)].into_iter().collect(),
        "exactly the changed and the new record"
    );
    assert_eq!(n, 2, "two records encoded");
    // Header (sequence, delta), then the one removal, then records.
    assert_eq!(&out[..6], &[0x08, 7, 0x28, 1, 0x18, 1]);

    // The ledger now holds `now`: the same content again is silent, and
    // silence leaves the buffer exactly as it was.
    let mut out = BytesMut::from(&b"xy"[..]);
    assert_eq!(
        ledger.emit_delta(&FixCodec, 3, 8, &now, &mut out, &mut n),
        Emitted::Silent
    );
    assert_eq!(&out[..], b"xy", "no bytes on silence");
}

/// A group not asked for on the previous step had no members: its next
/// frame is a full again (its old clients are gone, the new ones have
/// no baseline) — and the full re-syncs the ledger.
#[test]
fn a_step_gap_makes_the_group_fresh() {
    let mut ledger = established(&[(1, 0, 0)]);
    let (mut out, mut n) = (BytesMut::new(), 0);
    let now = content(&[(1, 0, 0), (2, 3, 3)]);
    assert_eq!(
        ledger.emit_delta(&FixCodec, 3, 3, &now, &mut out, &mut n),
        Emitted::Full,
        "step 2 was skipped"
    );
    let s = decode(&out);
    assert!(!s.delta);
    assert_eq!(upserts(&s), [(1, 0, 0), (2, 3, 3)].into_iter().collect());
    assert!(ledger.full_sent(3) && !ledger.full_sent(4));
    out.clear();
    assert_eq!(
        ledger.emit_delta(&FixCodec, 4, 4, &now, &mut out, &mut n),
        Emitted::Silent,
        "the full re-synced the ledger"
    );
}

/// The full frame is encoded once per step and shared (fresh frame,
/// keep-alive, private one-shots); a new step encodes it again.
#[test]
fn the_full_frame_is_encoded_once_per_step() {
    let mut ledger: SetLedger<WirePos> = SetLedger::default();
    let now = content(&[(1, 0, 0), (2, 1, 1)]);
    let mut n = 0;
    let a = ledger.full_frame(&FixCodec, 5, 9, &now, &mut n);
    let b = ledger.resync(&FixCodec, 5, 9, &now, &mut n);
    assert_eq!(a.as_ptr(), b.as_ptr(), "the same bytes, shared");
    assert_eq!(n, 2, "encoded once");
    let c = ledger.full_frame(&FixCodec, 6, 10, &now, &mut n);
    assert_ne!(a, c, "a new step, a new sequence");
    assert_eq!(n, 4);
    assert_eq!(decode(&c).sequence, 10);
}

/// The full mode is the full-only rooms' frame: silence on unchanged
/// content, else the whole content under the full header — byte for
/// byte what the inline encoders wrote (same iteration order).
#[test]
fn the_full_mode_is_the_full_only_frame() {
    let mut ledger: SetLedger<WirePos> = SetLedger::default();
    let now = content(&[(1, 0, 0), (2, -4, 7), (300, 12, -1)]);
    let (mut out, mut n) = (BytesMut::new(), 0);
    assert!(ledger.emit_full(&FixCodec, 42, &now, &mut out, &mut n));
    let mut expected = BytesMut::new();
    write_full_header(&mut expected, 42);
    put_entity_records(
        &FixCodec,
        now.iter().map(|(id, wire)| (*id, wire)),
        &mut expected,
    );
    assert_eq!(out, expected);
    assert_eq!(n, 3);
    out.clear();
    assert!(!ledger.emit_full(&FixCodec, 43, &now, &mut out, &mut n));
    assert!(out.is_empty());
}

/// A player is owed one one-shot full per group it has no baseline for,
/// none when the group's own full preceded the frame, and a new one
/// after `forget` (a resume).
#[test]
fn baselines_owe_one_full_per_new_group() {
    let mut b = Baselines::default();
    let p = PlayerId(7);
    assert!(b.owed(p, 0u8, false), "a join");
    assert!(!b.owed(p, 0, false), "baselined");
    assert!(!b.owed(p, 1, true), "a group change into a full: covered");
    assert!(!b.owed(p, 1, false));
    assert!(b.owed(p, 0, false), "back to the first group");
    b.forget(p);
    assert!(b.owed(p, 0, false), "a resumed session");
}

mod rate;
