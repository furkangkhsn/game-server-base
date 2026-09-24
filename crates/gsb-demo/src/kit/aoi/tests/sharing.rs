//! One encoding per cell, shared by reference with every group that
//! can see it — and the protobuf concatenation that makes it legal.

use super::*;

mod delta;

/// The full/delta decision is per (group, cell): two groups sharing a
/// cell that becomes empty get the SAME single cell-exit record, and
/// a cell that re-appears is delivered as full records to groups
/// without a baseline.
#[test]
fn aoi_cell_exit_is_one_record_and_shared() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    // P1 in Cell(0,0), P2 in Cell(1,0): both groups' 3×3 include
    // Cell(0,1) (the test cell below).
    let _p1 = place(&mut world, &mut room, ConnectionId(1), 10.0, 0.0);
    let _p2 = place(&mut world, &mut room, ConnectionId(2), 30.0, 0.0);
    room.update(&mut world, &ctx(1));

    // Tick 2: an entity in Cell(0,1) — inside BOTH groups' 3×3.
    let e = world
        .spawn((Position { x: 5.0, y: 25.0 }, Speed(DEFAULT_SPEED)))
        .id();
    room.update(&mut world, &ctx(2));

    // Tick 3: the entity leaves Cell(0,1) (to a far cell) — the cell
    // becomes empty; BOTH groups' packets carry exactly one
    // cell-exit record for it (one record, not one per entity), and
    // no entity records for it at all.
    let ent = world
        .entity(e)
        .get::<crate::kit::identity::WireId>()
        .expect("stamped")
        .get();
    world.entity_mut(e).insert(Position { x: 5.0, y: 400.0 });
    room.update(&mut world, &ctx(3));

    for group in [Cell(0, 0), Cell(1, 0)] {
        let mut out = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx(3), &group, &[], &mut out),
            "the departure re-emits for group {group:?}"
        );
        let snap = decode(&out);
        assert!(snap.delta);
        assert_eq!(
            snap.cell_exits.len(),
            1,
            "one cell-exit record for the emptied cell (group {group:?}): {snap:?}"
        );
        let exit: &CellExit = &snap.cell_exits[0];
        assert_eq!(
            (exit.x, exit.y),
            (0, 1),
            "the exited cell is named: {snap:?}"
        );
        assert!(
            snap.entities.iter().all(|r| r.entity != ent),
            "the departed entity is not re-carried (group {group:?})"
        );
    }
}

/// The encoded-records metric counts each entity's piece once per
/// tick (the overlap collapse): two groups sharing one populated cell
/// encode that cell's records once, not twice.
#[test]
fn aoi_encoding_is_once_per_cell_not_per_group() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    // Two groups (two cells) sharing the populated Cell(0,0).
    let _a = place(&mut world, &mut room, ConnectionId(1), 10.0, 0.0); // Cell(0,0)
    let _b = place(&mut world, &mut room, ConnectionId(2), 30.0, 0.0); // Cell(1,0)
    let npc = world
        .spawn((Position { x: 5.0, y: 5.0 }, Speed(DEFAULT_SPEED)))
        .id();
    room.update(&mut world, &ctx(1));

    // Tick 1: both groups are fresh → both fulls encode Cell(0,0)'s
    // two records ONCE (the full piece is shared).
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out));
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(1, 0), &[], &mut out));
    assert_eq!(
        room.encoded_records(),
        3,
        "2 shared + 1 exclusive, each once"
    );

    // Tick 2: static → silence, nothing encoded.
    room.update(&mut world, &ctx(2));
    assert!(!room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out));
    assert!(!room.snapshot(&mut world, &ctx(2), &Cell(1, 0), &[], &mut out));
    assert_eq!(room.encoded_records(), 0, "silence encodes nothing");

    // Tick 3: the NPC moves within its cell → one delta piece for
    // Cell(0,0) (shared by both groups): 1 record encoded.
    world.entity_mut(npc).insert(Position { x: 8.0, y: 5.0 });
    room.update(&mut world, &ctx(3));
    assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &[], &mut out));
    assert!(room.snapshot(&mut world, &ctx(3), &Cell(1, 0), &[], &mut out));
    assert_eq!(
        room.encoded_records(),
        1,
        "the shared cell's delta is encoded once"
    );
}

/// The pieces concatenate into a decodable `WorldSnapshot`: header +
/// several cells' pre-encoded blocks decode to the union (the
/// verified merge property the design relies on).
#[test]
fn aoi_concatenated_pieces_are_valid_protobuf() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    // One entity per cell in three adjacent cells.
    let a = place(&mut world, &mut room, ConnectionId(1), 5.0, 5.0); // Cell(0,0)
    let b = place(&mut world, &mut room, ConnectionId(2), 25.0, 5.0); // Cell(1,0)
    let c = place(&mut world, &mut room, ConnectionId(3), 45.0, 5.0); // Cell(2,0)
    room.update(&mut world, &ctx(1));

    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(1, 0), &[], &mut out));
    let snap = decode(&out);
    let seen = ids(&snap);
    assert!(
        seen.contains(&a) && seen.contains(&b) && seen.contains(&c),
        "the group's packet is the union of the three cells' blocks: {seen:?}"
    );
    assert_eq!(
        snap.entities.len(),
        3,
        "exactly the union, no duplicates: {snap:?}"
    );
}

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
    let _a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
    let ent = *room.player_entity.get(&PlayerId(1)).unwrap();

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
    let _a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
    let ent = *room.player_entity.get(&PlayerId(1)).unwrap();

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
//    structural (bevy's write path — no `bump()` discipline), the
//    change list is the diff (partial deltas, quantization no-ops),
//    and the bookkeeping it maintains (leaves, births, same-tick
//    churn) is order-independent and leak-free.
