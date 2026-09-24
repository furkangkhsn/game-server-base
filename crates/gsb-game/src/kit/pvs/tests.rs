//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

//! Logic-level PVS tests (precise, direct `SectorRoom` calls; they
//! need the room's private bookkeeping, so they live here rather than
//! in `tests/pvs.rs`). Geometry: see the module docs ("The map").

use std::collections::BTreeSet;
use std::time::Duration;

use crate::kit::seam::{DEFAULT_SPEED, Position, SECTOR_EAST, SECTOR_NW, SECTOR_WEST, Speed};
use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::room::TickCtx;

use super::*;
use gsb_core::id::PlayerId;
use gsb_core::room::GameLogic;
use prost::Message;

/// The instantiation these tests drive: the demo game over the demo map
/// in the kit's convex-sector preset (shadows the generic room of
/// `use super::*`).
type SectorRoom =
    super::SectorRoom<crate::kit::seam::DemoGame, crate::kit::space::ConvexSectors2<Position>>;

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

/// Join a player (wire id assigned) and move its entity to an exact
/// position for a deterministic sector placement. Returns the wire id.
fn place(world: &mut World, room: &mut SectorRoom, conn: ConnectionId, x: f32, y: f32) -> u64 {
    let admission = room.on_join(world, conn);
    let entity = *room
        .player_entity
        .get(&admission.player)
        .expect("registered");
    world.entity_mut(entity).insert(Position { x, y });
    admission.entity
}

fn snap_ids(out: &bytes::BytesMut) -> BTreeSet<u64> {
    crate::kit::seam::WorldSnapshot::decode(out.as_ref())
        .expect("snapshot payload")
        .entities
        .iter()
        .map(|e| e.entity)
        .collect()
}

/// The test that separates PVS from distance-based AOI: two entities
/// **3 units apart** (sector A at x=-1, sector B at x=+2, the wall at
/// x=0 between them) do NOT see each other, because A and B are
/// unlinked in the visibility table. A distance-AOI with any radius
/// >= 3 would let them see each other and cannot pass this test.
#[test]
fn unlinked_sectors_invisible_at_3_units() {
    let mut world = World::new();
    let mut room = SectorRoom::new();

    let p1 = place(&mut world, &mut room, ConnectionId(1), -1.0, 0.0); // A
    let p2 = place(&mut world, &mut room, ConnectionId(2), 2.0, 0.0); // B
    room.update(&mut world, &ctx(1));

    // Pin the scenario: the two are exactly 3 units apart.
    assert_eq!(
        ((-1.0f32 - 2.0).abs(), (0.0f32 - 0.0).abs()),
        (3.0f32, 0.0f32),
        "the scenario is 3 units apart"
    );

    let mut out_a = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_WEST), &[], &mut out_a));
    let a = snap_ids(&out_a);
    assert!(a.contains(&p1), "P1 in A's snapshot: {a:?}");
    assert!(
        !a.contains(&p2),
        "P2 (3 units away, unlinked sector) NOT visible: {a:?}"
    );

    let mut out_b = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_EAST), &[], &mut out_b));
    let b = snap_ids(&out_b);
    assert!(b.contains(&p2), "P2 in B's snapshot: {b:?}");
    assert!(
        !b.contains(&p1),
        "P1 (3 units away, unlinked sector) NOT visible: {b:?}"
    );
}

/// Linked sectors are visible — regardless of distance: A↔C (15 units
/// apart, open passage) and C↔D (7 units apart, north sightline).
#[test]
fn linked_sectors_visible() {
    let mut world = World::new();
    let mut room = SectorRoom::new();

    // The north band is split at x = -10: C covers x ∈ [-50, -10],
    // D covers x ∈ [-10, 50] — so "a point in C" needs x <= -10.
    let p1 = place(&mut world, &mut room, ConnectionId(1), -1.0, 10.0); // A
    let p2 = place(&mut world, &mut room, ConnectionId(2), -20.0, 25.0); // C (~24 from p1)
    let q1 = place(&mut world, &mut room, ConnectionId(3), -20.0, 30.0); // C
    let q2 = place(&mut world, &mut room, ConnectionId(4), 2.0, 30.0); // D (~22 from q1)
    room.update(&mut world, &ctx(1));

    let mut out_a = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_WEST), &[], &mut out_a));
    let a = snap_ids(&out_a);
    // A sees A and C (both linked): p1, p2, q1 — but NOT q2 (in D,
    // unlinked with A).
    assert!(
        a.contains(&p1) && a.contains(&p2) && a.contains(&q1),
        "A sees A and C (linked): {a:?}"
    );
    assert!(!a.contains(&q2), "A does not see D (unlinked): {a:?}");

    let mut out_c = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_NW), &[], &mut out_c));
    let c = snap_ids(&out_c);
    assert!(
        c.contains(&p1) && c.contains(&p2) && c.contains(&q1) && c.contains(&q2),
        "C sees A and D (both linked): {c:?}"
    );
}

/// Same sector is always visible — even at the sector's far corners
/// (~67 units apart): the group is the sector, and a sector always
/// sees itself. This is not a distance test (a small-radius AOI would
/// hide these two).
#[test]
fn same_sector_always_visible_at_far_corners() {
    let mut world = World::new();
    let mut room = SectorRoom::new();

    let p1 = place(&mut world, &mut room, ConnectionId(1), -49.0, -49.0); // A, SW corner
    let p2 = place(&mut world, &mut room, ConnectionId(2), -1.0, 19.0); // A, NE corner
    room.update(&mut world, &ctx(1));

    let mut out_a = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_WEST), &[], &mut out_a));
    let a = snap_ids(&out_a);
    assert!(
        a.contains(&p1) && a.contains(&p2),
        "co-sector residents visible: {a:?}"
    );
}

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
    let entity_p1 = *room.player_entity.get(&PlayerId(1)).unwrap();
    world
        .entity_mut(entity_p1)
        .insert(Position { x: -20.0, y: 25.0 });
    room.update(&mut world, &ctx(2));

    let mut out_c = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(2), &Sector(SECTOR_NW), &[], &mut out_c),
        "new sector re-emits"
    );
    let snap_c = crate::kit::seam::WorldSnapshot::decode(out_c.as_ref()).expect("snapshot");
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
    let snap_a = crate::kit::seam::WorldSnapshot::decode(out_a2.as_ref()).expect("snapshot");
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

/// "No change" contract (self-contained snapshots, no delta/history):
/// a sector snapshot whose wire content is identical across two ticks
/// is not re-encoded; a position change flips it back to "changed".
#[test]
fn sector_no_change_when_static() {
    let mut world = World::new();
    let mut room = SectorRoom::new();

    let _ = place(&mut world, &mut room, ConnectionId(1), 7.0, 9.0); // B, no MoveTarget
    let sector = Sector(SECTOR_EAST);

    room.update(&mut world, &ctx(1));
    let mut out1 = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(1), &sector, &[], &mut out1),
        "first emit"
    );

    room.update(&mut world, &ctx(2));
    let mut out2 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx(2), &sector, &[], &mut out2),
        "static snapshot silent"
    );
    assert!(out2.is_empty(), "no bytes written on silence");

    let entity = *room.player_entity.get(&PlayerId(1)).unwrap();
    world
        .entity_mut(entity)
        .insert(Position { x: 7.0, y: 19.0 });
    room.update(&mut world, &ctx(3));
    let mut out3 = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(3), &sector, &[], &mut out3),
        "movement re-emits"
    );
}

/// Broadcast set + OUT: an entity with a `Position` but no `WireId`
/// (spawned outside `on_join`) is stamped in `update` and appears in
/// its sector's snapshot; an entity OUTSIDE the map (a runaway target)
/// lands in `SECTOR_OUT`, sees only itself, and leaks into no map
/// sector's snapshot — the broadcast set stays exactly "has a
/// `Position`".
#[test]
fn orphan_stamped_and_outside_map_is_contained() {
    let mut world = World::new();
    let mut room = SectorRoom::new();

    let a = place(&mut world, &mut room, ConnectionId(1), 7.0, 9.0); // B
    let _orphan_in = world
        .spawn((Position { x: 8.0, y: 10.0 }, Speed(DEFAULT_SPEED)))
        .id(); // B
    let runaway = world
        .spawn((Position { x: 300.0, y: 300.0 }, Speed(DEFAULT_SPEED)))
        .id(); // outside the map
    room.update(&mut world, &ctx(1));

    let mut out_b = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_EAST), &[], &mut out_b));
    let b = snap_ids(&out_b);
    assert_eq!(
        b.len(),
        2,
        "B: resident + stamped orphan (the runaway is not here): {b:?}"
    );
    assert!(b.contains(&a));

    let mut out_out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_OUT), &[], &mut out_out));
    let out_ids = snap_ids(&out_out);
    let runaway_id = room
        .last
        .get(&Sector(SECTOR_OUT))
        .and_then(|m| m.iter().next().map(|(k, _)| *k))
        .expect("OUT has exactly one occupant (the runaway)");
    assert!(
        out_ids.contains(&runaway_id),
        "OUT sees its occupant: {out_ids:?}"
    );
    // The runaway must not appear in ANY map sector's snapshot.
    for s in 0..4u8 {
        let mut o = bytes::BytesMut::new();
        room.snapshot(&mut world, &ctx(1), &Sector(s), &[], &mut o);
        let ids = snap_ids(&o);
        assert!(
            !ids.contains(&runaway_id),
            "sector {s} must not leak the runaway: {ids:?}"
        );
    }
    let _ = runaway;
}

mod many_sectors;
