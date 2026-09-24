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
        .get::<crate::identity::WireId>()
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

// ── Change-detection tests (round item (b)): the bookkeeping the
//    dirty marking maintains (leaves, births, same-tick churn) is
//    order-independent and leak-free. (The value pins of this item —
//    structural dirty marking, partial deltas, quantization no-ops —
//    and of item (a), the cache invalidation, read the record the
//    game's codec wrote: they run on the demo game, in gsb-demo.)
