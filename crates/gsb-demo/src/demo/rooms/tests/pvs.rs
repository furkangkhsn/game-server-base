//! The PVS room over the demo map: a sector crossing, read back as the
//! demo's record at its new position.

use super::*;
use crate::demo::sectors::{SECTOR_EAST, SECTOR_NW, SECTOR_WEST};
use crate::pvs::{Sector, SectorRoom};

/// Sector transition: an entity crossing a boundary changes group; its
/// record moves to the new sector's snapshot at the new position with
/// the same wire identity, stays visible wherever the new sector is
/// linked (A sees C), and remains hidden where the wall still stands
/// (B does not see C) — the identity invariant across the PVS
/// boundary crossing.
#[test]
fn sector_transition_snapshot_and_identity() {
    let mut world = World::new();
    let mut room = SectorRoom::new();

    let p1 = place(&mut world, &mut room, ConnectionId(1), -1.0, 0.0); // A
    let p2 = place(&mut world, &mut room, ConnectionId(2), 2.0, 0.0); // B
    room.update(&mut world, &ctx(1));

    let mut out_a = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_WEST), &[], &mut out_a));
    assert!(snap_ids(&out_a).contains(&p1));

    // P1 moves into sector C (x <= -10, linked with A, NOT with B).
    let entity_p1 = entity_of(&mut world, p1);
    world
        .entity_mut(entity_p1)
        .insert(Position { x: -20.0, y: 25.0 });
    room.update(&mut world, &ctx(2));

    let mut out_c = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(2), &Sector(SECTOR_NW), &[], &mut out_c),
        "new sector re-emits"
    );
    let snap_c = WorldSnapshot::decode(out_c.as_ref()).expect("snapshot");
    let now_c: BTreeSet<u64> = snap_c.entities.iter().map(|e| e.entity).collect();
    assert!(now_c.contains(&p1), "P1 in C's snapshot: {now_c:?}");
    let rec_c = snap_c
        .entities
        .iter()
        .find(|e| e.entity == p1)
        .expect("P1's record");
    assert_eq!(
        (rec_c.x, rec_c.y),
        (-20, 25),
        "P1 at its new position in C's snapshot"
    );

    // A sees C (linked): P1 stays in A's snapshot — its record MOVED
    // to the new position under the same wire id (C is linked with A,
    // so the crossing did not end A's visibility of P1).
    let mut out_a2 = bytes::BytesMut::new();
    room.snapshot(&mut world, &ctx(2), &Sector(SECTOR_WEST), &[], &mut out_a2);
    let snap_a = WorldSnapshot::decode(out_a2.as_ref()).expect("snapshot");
    let rec_a = snap_a
        .entities
        .iter()
        .find(|e| e.entity == p1)
        .expect("P1 still visible from A (A and C are linked)");
    assert_eq!(
        (rec_a.x, rec_a.y),
        (-20, 25),
        "P1's record moved, id unchanged: {rec_a:?}"
    );

    let mut out_b2 = bytes::BytesMut::new();
    room.snapshot(&mut world, &ctx(2), &Sector(SECTOR_EAST), &[], &mut out_b2);
    let now_b = snap_ids(&out_b2);
    assert!(
        !now_b.contains(&p1),
        "P1 (now in C) is still invisible to B — the wall did not move: {now_b:?}"
    );
    assert!(now_b.contains(&p2), "B still sees itself: {now_b:?}");
    // Identity: P1's wire id is the one assigned at join, unchanged
    // across the sector crossing.
    assert_eq!(p1, 1, "first entity gets wire id 1");
    assert!(now_c.contains(&1), "P1 keeps wire id 1 in C's snapshot");
}
