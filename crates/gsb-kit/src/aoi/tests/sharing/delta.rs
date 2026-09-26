//! Delta correctness: only real changes are recorded, a quantized
//! move that does not change the wire is silent, and a leave shows up
//! as both a removal and a cell exit.

use super::*;

/// (b) the despawn path: a leave is not a component write — the
/// removal is parked in `on_leave` and applied by `update`. The
/// cell's delta carries the leaver's wire id in `removed`; when the
/// cell empties, one `CellExit` supersedes the per-entity records.
#[test]
fn aoi_leave_removal_in_delta_and_cell_exit() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);
    let w1 = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
    let w2 = place(&mut world, &mut room, ConnectionId(2), 10.0, 0.0); // Cell(0,0)
    room.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out));
    assert_eq!(ids(&decode(&out)).len(), 2);
    assert_eq!(room.encoded_records(), 2);

    // One leaves: the delta carries exactly its wire id in `removed`
    // (the remaining entity is not re-carried).
    room.on_leave(&mut world, PlayerId(1));
    room.update(&mut world, &ctx(2));
    let mut out2 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out2));
    let s2 = decode(&out2);
    assert!(s2.delta);
    assert_eq!(
        s2.removed.len(),
        1,
        "the leaver's wire id is removed: {s2:?}"
    );
    assert_eq!(s2.removed[0], w1);
    assert!(
        !s2.removed.contains(&w2),
        "the remaining entity is NOT removed: {s2:?}"
    );
    assert!(
        s2.entities.is_empty(),
        "the remaining entity is not re-carried: {s2:?}"
    );
    assert_eq!(room.encoded_records(), 0, "exits are not 'encoded records'");

    // The last one leaves: a `CellExit` supersedes the per-entity
    // record (the client forgets the whole cell in one record).
    room.on_leave(&mut world, PlayerId(2));
    room.update(&mut world, &ctx(3));
    let mut out3 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &[], &mut out3));
    let s3 = decode(&out3);
    assert_eq!(s3.cell_exits.len(), 1, "one cell-exit record: {s3:?}");
    assert_eq!((s3.cell_exits[0].x, s3.cell_exits[0].y), (0, 0));
    assert!(
        s3.removed.is_empty(),
        "the cell-exit supersedes the entity records: {s3:?}"
    );
    assert!(s3.entities.is_empty());
}

/// (b) the birth rule's generalization (module docs, "Dirty cells"):
/// a cell that already holds non-member content (an NPC) and gains
/// its first MEMBER this tick is a fresh group — the member is
/// baselined by the group's own full (batch-ordered ahead of the
/// private frame), so the one-shot private full is skipped. (The
/// spec's literal bucket-diff formulation would miss this birth —
/// the cell was already in the previous buckets — leaving the member
/// to the private full; both paths baseline correctly, and the
/// member-count rule is the one that also covers the general case.)
#[test]
fn aoi_member_join_npc_cell_born_full() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    // Tick 1: an NPC-only cell (no members anywhere yet).
    let npc = world
        .spawn((Position { x: 40.0, y: 0.0 }, Speed(DEFAULT_SPEED)))
        .id(); // Cell(2,0)
    room.update(&mut world, &ctx(1));

    // Tick 2: a member joins and lands in the NPC's cell.
    let m = place(&mut world, &mut room, ConnectionId(7), 44.0, 0.0); // Cell(2,0)
    room.update(&mut world, &ctx(2));
    assert!(
        room.book.born_groups.contains(&Cell(2, 0)),
        "a member joining an NPC-held cell is a fresh group (member count 0 → 1)"
    );
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(2), &Cell(2, 0), &[], &mut out));
    let s = decode(&out);
    assert!(!s.delta, "the fresh group's first packet is a full");
    let seen = ids(&s);
    let npc_wire = world.entity(npc).get::<WireId>().expect("stamped").get();
    assert!(
        seen.contains(&m) && seen.contains(&npc_wire),
        "the full carries member + NPC: {seen:?}"
    );
    // The member is baselined by that full: no private frame.
    let mut pbuf = bytes::BytesMut::new();
    assert!(
        !room.private(&mut world, PlayerId(7), &Cell(2, 0), &[], &mut pbuf),
        "the group's full already baselined the member — no private frame"
    );
    assert!(room.baselines.holds(PlayerId(7)));
}

/// (b) a join+leave within one tick: the entity never enters
/// `last_cell` (no `update` ran between the two), so `on_leave`
/// parks no removal, the dirty query never sees the despawned
/// entity, and no bookkeeping is left behind — counts, buckets, and
/// the established group's stream are exactly as before.
#[test]
fn aoi_join_leave_same_tick_inert() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);
    let _m = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
    room.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out)); // fresh full

    // A connection that joins AND leaves before the next update.
    let joined = room.on_join(&mut world, ConnectionId(9));
    room.on_leave(&mut world, joined.player);
    assert!(
        room.book.pending_removals.is_empty(),
        "no removal to park (the entity was never bucketed)"
    );

    room.update(&mut world, &ctx(2));
    assert_eq!(
        *room
            .book
            .member_counts
            .get(&Cell(0, 0))
            .expect("member count"),
        1,
        "the member counts are intact"
    );
    let mut out2 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out2),
        "the phantom join produced no delta"
    );
    assert_eq!(room.encoded_records(), 0);
}
