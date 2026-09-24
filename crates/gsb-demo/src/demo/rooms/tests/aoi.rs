//! The AOI room over the demo game (`cell_size = 20`): the cell-delta
//! engine's caches and change detection, pinned through the demo codec's
//! record values and its truncation.

use super::*;
use crate::aoi::{AoiRoom, Cell};

// ── Cache-invalidation tests (round item (a)): the per-tick
//    classification/piece caches must never serve a stale answer —
//    across ticks (different content → different blocks) or within
//    the silent/delta alternation (a stale delta must not be
//    re-served, and two groups asking the same cell get the same
//    block).

/// (a) two consecutive ticks with different content produce
/// different blocks: tick N+1's piece is never tick N's cached
/// piece (the per-tick caches are cleared in `update`).
#[test]
fn aoi_tick_cache_no_stale_block() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);
    let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
    let ent = entity_of(&mut world, a);

    room.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out)); // fresh full

    // Tick 2: the entity moves within its cell → a delta with (15,0).
    world.entity_mut(ent).insert(Position { x: 15.0, y: 0.0 });
    room.update(&mut world, &ctx(2));
    let mut out2 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out2));
    let s2 = decode(&out2);
    assert!(s2.delta);
    assert_eq!(s2.entities.len(), 1, "exactly the moved record: {s2:?}");
    assert_eq!((s2.entities[0].x, s2.entities[0].y), (15, 0));
    assert_eq!(room.encoded_records(), 1);

    // Tick 3: it moves again → a DIFFERENT block ((10,0)) — tick 2's
    // cached piece/classification must not be replayed.
    world.entity_mut(ent).insert(Position { x: 10.0, y: 0.0 });
    room.update(&mut world, &ctx(3));
    let mut out3 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &[], &mut out3));
    let s3 = decode(&out3);
    assert!(s3.delta);
    assert_eq!(s3.entities.len(), 1, "exactly the moved record: {s3:?}");
    assert_eq!(
        (s3.entities[0].x, s3.entities[0].y),
        (10, 0),
        "tick 3 must not serve tick 2's cached block"
    );
    assert_eq!(room.encoded_records(), 1);
}

/// (a) a cell that goes silent after a delta tick ships nothing —
/// the stale delta is not re-served; the keep-alive full carries the
/// CURRENT content (never a stale piece).
#[test]
fn aoi_silent_delta_silent_no_stale() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);
    let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
    let ent = entity_of(&mut world, a);

    room.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out)); // fresh full

    // Tick 2: silence — an established group ships NOTHING.
    room.update(&mut world, &ctx(2));
    let mut out2 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out2),
        "silence ships nothing (no stale full, no delta)"
    );
    assert!(out2.is_empty());
    assert_eq!(room.encoded_records(), 0);

    // Tick 3: a delta tick — the silence of tick 2 must not
    // suppress the cell's (fresh) piece.
    world.entity_mut(ent).insert(Position { x: 19.0, y: 0.0 });
    room.update(&mut world, &ctx(3));
    let mut out3 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &[], &mut out3));
    let s3 = decode(&out3);
    assert!(s3.delta);
    assert_eq!(s3.entities.len(), 1);
    assert_eq!((s3.entities[0].x, s3.entities[0].y), (19, 0));
    assert_eq!(room.encoded_records(), 1);

    // Tick 4: silence again — the stale tick-3 delta is not
    // replayed, and the keep-alive full carries the current content.
    room.update(&mut world, &ctx(4));
    let mut out4 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx(4), &Cell(0, 0), &[], &mut out4),
        "the stale delta must not be re-served"
    );
    assert_eq!(room.encoded_records(), 0);
    let mut ka = bytes::BytesMut::new();
    assert!(room.keepalive(&mut world, &ctx(4), &Cell(0, 0), None, &mut ka));
    let s4 = decode(&ka);
    assert!(!s4.delta, "the keep-alive ships a full");
    assert_eq!(s4.entities.len(), 1);
    assert_eq!(
        (s4.entities[0].x, s4.entities[0].y),
        (19, 0),
        "the full carries the current content, not a stale piece"
    );
}

/// (a) two different groups that both see the same cell in the same
/// tick get the SAME block for it: content-identical records, and
/// the piece is encoded exactly once (shared by reference).
#[test]
fn aoi_two_groups_same_cell_same_block() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    // Two member groups (Cell(1,0) and Cell(3,0)) whose 3×3s overlap
    // in Cell(2,0); an NPC lives in Cell(2,0).
    let _m1 = place(&mut world, &mut room, ConnectionId(1), 20.0, 0.0); // Cell(1,0)
    let _m2 = place(&mut world, &mut room, ConnectionId(2), 60.0, 0.0); // Cell(3,0)
    let npc = world
        .spawn((Position { x: 40.0, y: 0.0 }, Speed(DEFAULT_SPEED)))
        .id(); // Cell(2,0)
    room.update(&mut world, &ctx(1));

    // Tick 2: the NPC moves within Cell(2,0) — both groups' deltas
    // must carry the identical block for that cell.
    world.entity_mut(npc).insert(Position { x: 45.0, y: 0.0 });
    room.update(&mut world, &ctx(2));

    let mut out_a = bytes::BytesMut::new();
    let mut out_b = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(2), &Cell(1, 0), &[], &mut out_a));
    assert!(room.snapshot(&mut world, &ctx(2), &Cell(3, 0), &[], &mut out_b));
    let s_a = decode(&out_a);
    let s_b = decode(&out_b);
    assert_eq!(
        s_a.entities.len(),
        1,
        "group A's delta carries only the shared cell's mover: {s_a:?}"
    );
    assert_eq!(
        s_b.entities.len(),
        1,
        "group B's delta carries only the shared cell's mover: {s_b:?}"
    );
    assert_eq!(
        (s_a.entities[0].entity, s_a.entities[0].x, s_a.entities[0].y),
        (s_b.entities[0].entity, s_b.entities[0].x, s_b.entities[0].y),
        "same cell, same tick → identical block"
    );
    assert_eq!(room.encoded_records(), 1, "one encoding, not one per group");
}

// ── Change-detection tests (round item (b)): the dirty marking is
//    structural (bevy's write path — no `bump()` discipline) and the
//    change list is the diff (partial deltas, quantization no-ops).

/// (b) structural dirty marking: a `Position` written by a writer
/// the room has no hook into (a direct `world.entity_mut` — no
/// system, no `MOVE_TO`, no invalidation call) still produces the
/// correct delta: the mark is set inside bevy's write path, so it
/// cannot be forgotten by any present or future writer.
#[test]
fn aoi_structural_dirty_direct_write() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);
    let _m = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
    // Two third-party entities the room never sees through
    // on_join/ingest (a co-resident keeps the source cell
    // non-empty, so the exit takes the per-entity `removed` path —
    // the whole-cell `CellExit` path is covered by
    // `aoi_leave_removal_in_delta_and_cell_exit`).
    let ghost = world
        .spawn((Position { x: 100.0, y: 0.0 }, Speed(DEFAULT_SPEED)))
        .id(); // Cell(5,0)
    world.spawn((Position { x: 110.0, y: 0.0 }, Speed(DEFAULT_SPEED))); // Cell(5,0)
    room.update(&mut world, &ctx(1));
    let ghost_wire = world.entity(ghost).get::<WireId>().expect("stamped").get();

    // The third-party writer moves the ghost across cells — the room
    // has no hook for this write; the delta must still carry the
    // exit (source cell) and the arrival (target cell).
    world
        .entity_mut(ghost)
        .insert(Position { x: 125.0, y: 0.0 }); // Cell(6,0)
    room.update(&mut world, &ctx(2));

    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(2), &Cell(5, 0), &[], &mut out));
    let s = decode(&out);
    assert!(s.delta);
    assert!(
        s.removed.contains(&ghost_wire),
        "the exit is recorded without any room hook: {s:?}"
    );

    let mut out2 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(2), &Cell(6, 0), &[], &mut out2));
    let s2 = decode(&out2);
    assert!(
        s2.entities
            .iter()
            .any(|e| e.entity == ghost_wire && (e.x, e.y) == (125, 0)),
        "the arrival is recorded: {s2:?}"
    );
}

/// (b) partial delta: a cell with five records of which ONE changed
/// ships exactly that one record — the other four are neither
/// re-carried nor re-encoded (the change list is the diff; no
/// per-cell content comparison runs).
#[test]
fn aoi_partial_delta_only_mover_recorded() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);
    // Five members, one cell (Cell(0,0): x = 3, 6, 9, 12, 15).
    let ws: Vec<u64> = (1..=5)
        .map(|i| {
            place(
                &mut world,
                &mut room,
                ConnectionId(i),
                (i as f32) * 3.0,
                0.0,
            )
        })
        .collect();
    room.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out));
    assert_eq!(
        ids(&decode(&out)).len(),
        5,
        "the fresh full carries all five"
    );
    assert_eq!(room.encoded_records(), 5);

    // Only member 3 (x=9) moves — a direct write (no MOVE_TO, no
    // system).
    let ent = entity_of(&mut world, ws[2]);
    world.entity_mut(ent).insert(Position { x: 18.0, y: 0.0 }); // still Cell(0,0)
    room.update(&mut world, &ctx(2));
    let mut out2 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out2));
    let s2 = decode(&out2);
    assert!(s2.delta);
    assert_eq!(s2.entities.len(), 1, "exactly the mover's record: {s2:?}");
    assert_eq!(s2.entities[0].entity, ws[2]);
    assert_eq!((s2.entities[0].x, s2.entities[0].y), (18, 0));
    assert!(s2.removed.is_empty(), "no exits: {s2:?}");
    assert_eq!(
        room.encoded_records(),
        1,
        "one encoding — the other four records are not re-encoded"
    );
}

/// (b) quantization: a move that leaves the WIRE position (i32
/// truncation) untouched produces no record — the cell stays silent
/// (no stale delta leaks) while the f32 world keeps moving; the
/// first wire-unit change does produce the record.
#[test]
fn aoi_quantized_move_no_wire_change_no_record() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);
    let m = place(&mut world, &mut room, ConnectionId(1), 0.5, 0.5); // wire (0,0), Cell(0,0)
    room.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out));

    // The f32 position changes; the wire position (0,0) does not.
    let ent = entity_of(&mut world, m);
    world.entity_mut(ent).insert(Position { x: 0.9, y: 0.5 });
    room.update(&mut world, &ctx(2));
    let mut out2 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out2),
        "an unchanged wire position is a no-op: the cell is silent, no stale delta"
    );
    assert_eq!(room.encoded_records(), 0);

    // Crossing the wire boundary (x: 0.9 → 1.0) flips the record.
    world.entity_mut(ent).insert(Position { x: 1.0, y: 0.5 });
    room.update(&mut world, &ctx(3));
    let mut out3 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &[], &mut out3));
    let s3 = decode(&out3);
    assert!(s3.delta);
    assert_eq!(s3.entities.len(), 1);
    assert_eq!((s3.entities[0].x, s3.entities[0].y), (1, 0));
    assert_eq!(room.encoded_records(), 1);
}
