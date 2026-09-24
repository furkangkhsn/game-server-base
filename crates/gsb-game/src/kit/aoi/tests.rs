//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

//! Logic-level AOI tests (precise, direct `AoiRoom` calls; they need
//! the room's private bookkeeping, so they live here rather than in
//! `tests/aoi.rs` — the room-actor-level behaviour, delta streams,
//! client views, and the loss-recovery bound live in `tests/aoi.rs`
//! and `tests/delta_aoi.rs`). `cell_size = 20` ⇒ (0,0)/(15,0) share
//! `Cell(0,0)`; (45,0) is `Cell(2,0)` (inside the 3×3 centered on
//! `Cell(1,0)`); (100,0) is `Cell(5,0)` (far).

use std::collections::BTreeSet;
use std::time::Duration;

use crate::kit::seam::{CellExit, WorldSnapshot};
use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::room::TickCtx;
use prost::Message;

use crate::kit::seam::{DEFAULT_SPEED, Position, Speed};

use super::*;
use crate::kit::identity::*;
use gsb_core::id::PlayerId;
use gsb_core::room::GameLogic;

mod sharing;

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

/// Join a player (wire id assigned) and move its entity to an exact
/// position for a deterministic cell placement. Returns the wire id.
fn place(world: &mut World, room: &mut AoiRoom, conn: ConnectionId, x: f32, y: f32) -> u64 {
    let admission = room.on_join(world, conn);
    let entity = *room
        .player_entity
        .get(&admission.player)
        .expect("registered");
    world.entity_mut(entity).insert(Position { x, y });
    admission.entity
}

fn decode(out: &bytes::BytesMut) -> WorldSnapshot {
    WorldSnapshot::decode(out.as_ref()).expect("snapshot payload")
}

fn ids(snap: &WorldSnapshot) -> BTreeSet<u64> {
    snap.entities.iter().map(|e| e.entity).collect()
}

/// A cell's full piece carries exactly the cell's records; a fresh
/// group's packet is a full (delta=false) of its 3×3.
#[test]
fn aoi_block_contains_near_not_far() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
    let b = place(&mut world, &mut room, ConnectionId(2), 15.0, 0.0); // Cell(0,0)
    let c = place(&mut world, &mut room, ConnectionId(3), 100.0, 0.0); // Cell(5,0)
    room.update(&mut world, &ctx(1));

    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out));
    let snap = decode(&out);
    assert!(!snap.delta, "a fresh group's first packet is a full");
    let near = ids(&snap);
    assert!(
        near.contains(&a) && near.contains(&b),
        "co-residents visible: {near:?}"
    );
    assert!(!near.contains(&c), "far cell must not be visible: {near:?}");

    let mut out2 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(5, 0), &[], &mut out2));
    let far = ids(&decode(&out2));
    assert!(far.contains(&c), "C sees itself: {far:?}");
    assert!(
        !far.contains(&a) && !far.contains(&b),
        "far C does not see A/B: {far:?}"
    );
}

/// Cell transition: an entity crossing a boundary changes group; the
/// new cell's DELTA carries it as an update, the old cell's delta
/// carries its exit (or a cell exit when the cell becomes empty) —
/// and its wire identity is unchanged throughout.
#[test]
fn aoi_cell_transition_block_and_identity() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
    let b = place(&mut world, &mut room, ConnectionId(2), 60.0, 0.0); // Cell(3,0)
    assert_ne!(a, b);

    room.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out));
    assert!(ids(&decode(&out)).contains(&a));

    // A moves into B's cell (Cell(3,0)); identity must be preserved.
    let entity_a = *room.player_entity.get(&PlayerId(1)).unwrap();
    world
        .entity_mut(entity_a)
        .insert(Position { x: 60.0, y: 0.0 });
    room.update(&mut world, &ctx(2));

    // Group Cell(3,0) is established (B since tick 1): its packet is
    // a delta; A is an update in it (B is unchanged and stays in the
    // client's baseline — the delta does not re-carry it).
    let mut out_b = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(2), &Cell(3, 0), &[], &mut out_b),
        "A's arrival re-emits (delta)"
    );
    let snap_b = decode(&out_b);
    assert!(snap_b.delta, "an established group ships a delta");
    let now_b = ids(&snap_b);
    assert!(
        now_b.contains(&a),
        "A is an update in the new cell's delta: {now_b:?}"
    );
    assert!(snap_b.removed.iter().all(|w| *w != a), "A is not exited");

    // Group Cell(0,0): A left its only resident cell → the cell
    // became empty → ONE cell-exit record (not per entity), no
    // entities re-carried.
    let mut out_a = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out_a),
        "A's departure re-emits (cell exit)"
    );
    let snap_a = decode(&out_a);
    assert!(snap_a.delta);
    assert!(snap_a.entities.is_empty(), "no entity records: {snap_a:?}");
    let exits: BTreeSet<(i32, i32)> = snap_a.cell_exits.iter().map(|e| (e.x, e.y)).collect();
    assert!(
        exits.contains(&(0, 0)),
        "the emptied cell is exited as one record: {exits:?}"
    );
    // Identity preserved across the cell move (the same wire id in
    // both cells' records).
    assert!(ids(&decode(&out_b)).contains(&a), "A keeps its wire id");
}

/// Late join: a player entering an already-populated cell's group
/// gets its full visibility block — now as a one-shot **private**
/// full (the group stays in delta mode for everyone; the group's own
/// packet is a delta and does not carry the co-residents again).
#[test]
fn aoi_late_join_sees_full_visibility_block() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0);
    let d = place(&mut world, &mut room, ConnectionId(2), 15.0, 0.0);
    room.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out));

    // B joins the same cell late (the group is established: its
    // packet is a delta that does NOT re-carry the co-residents).
    let b = place(&mut world, &mut room, ConnectionId(3), 5.0, 0.0);
    room.update(&mut world, &ctx(2));

    let mut out2 = bytes::BytesMut::new();
    // The group's own packet: silent for the unchanged residents
    // (B is a new entity → its cell's content changed → it is an
    // update; A and D are in the baseline, not re-carried).
    assert!(
        room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out2),
        "B's spawn re-emits (delta update)"
    );
    let group_delta = decode(&out2);
    assert!(group_delta.delta);
    assert!(
        ids(&group_delta).contains(&b),
        "the group delta carries B (new): {:?}",
        ids(&group_delta)
    );

    // The one-shot private full: B's entire visibility block
    // (co-residents + itself, full mode).
    let mut priv_out = bytes::BytesMut::new();
    assert!(
        room.private(&mut world, PlayerId(3), &Cell(0, 0), &[], &mut priv_out),
        "a late joiner receives the one-shot full"
    );
    let full = crate::kit::seam::Private::decode(priv_out.as_ref()).expect("private frame");
    let snap = match full.payload {
        Some(crate::kit::seam::private::Payload::Snapshot(s)) => s,
        other => panic!("expected the snapshot oneof, got {other:?}"),
    };
    assert!(!snap.delta, "the one-shot full is a full snapshot");
    let seen = ids(&snap);
    assert!(
        seen.contains(&a) && seen.contains(&d),
        "sees co-residents: {seen:?}"
    );
    assert!(seen.contains(&b), "sees itself: {seen:?}");

    // A second private call the same tick (or a later tick with no
    // group change): no full again (only an ack, if any).
    let mut priv_out2 = bytes::BytesMut::new();
    assert!(
        !room.private(&mut world, PlayerId(3), &Cell(0, 0), &[], &mut priv_out2),
        "the one-shot full is one-shot"
    );
}

/// Broadcast set: an entity spawned with a `Position` but no `WireId`
/// (a bullet/NPC, not via `on_join`) is stamped in `update` and
/// appears in its cell's full. The broadcast set is exactly "has a
/// `Position`".
#[test]
fn aoi_broadcast_set_position_is_stamped() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0);
    let _orphan = world
        .spawn((Position { x: 3.0, y: 3.0 }, Speed(DEFAULT_SPEED)))
        .id();
    room.update(&mut world, &ctx(1));

    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out));
    let snap = decode(&out);
    let ids = ids(&snap);
    assert!(ids.contains(&a), "resident present: {ids:?}");
    assert_eq!(
        ids.len(),
        2,
        "orphan stamped and broadcast (2 entities): {ids:?}"
    );
    assert!(!ids.contains(&0), "wire ids start at 1");
}

/// "No change" contract: a 3×3 whose content is identical across two
/// ticks is not re-encoded (silence writes no bytes); a position
/// change flips the cell to a delta.
#[test]
fn aoi_no_change_when_block_static() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    let _ = place(&mut world, &mut room, ConnectionId(1), 7.0, 9.0); // no MoveTarget
    let cell = Cell(0, 0);

    room.update(&mut world, &ctx(1));
    let mut out1 = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(1), &cell, &[], &mut out1),
        "first emit (full)"
    );

    room.update(&mut world, &ctx(2));
    let mut out2 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx(2), &cell, &[], &mut out2),
        "static 3×3 silent"
    );
    assert!(out2.is_empty(), "no bytes written on silence");

    let entity = *room.player_entity.get(&PlayerId(1)).unwrap();
    world
        .entity_mut(entity)
        .insert(Position { x: 7.0, y: 19.0 });
    room.update(&mut world, &ctx(3));
    let mut out3 = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(3), &cell, &[], &mut out3),
        "movement re-emits (delta)"
    );
    let snap3 = decode(&out3);
    assert!(snap3.delta, "the re-emit is a delta");
    assert_eq!(snap3.entities.len(), 1, "the delta carries the one mover");
    assert_eq!(snap3.removed.len(), 0, "no exits");
}

/// The keep-alive path for an unchanged group is a freshly encoded
/// FULL of the group's view (not a re-send of the last delta).
#[test]
fn aoi_keepalive_ships_fresh_full() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);

    let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0);
    let b = place(&mut world, &mut room, ConnectionId(2), 15.0, 0.0);
    room.update(&mut world, &ctx(1));
    let cell = Cell(0, 0);
    let mut out1 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &cell, &[], &mut out1));

    // Static: the group is silent, so the keep-alive fires.
    room.update(&mut world, &ctx(2));
    let last = out1.clone().freeze();
    let mut ka = bytes::BytesMut::new();
    assert!(
        room.keepalive(&mut world, &ctx(2), &cell, Some(&last), &mut ka),
        "the delta-mode logic overrides the keep-alive"
    );
    let snap = decode(&ka);
    assert!(!snap.delta, "the keep-alive payload is a full");
    let seen = ids(&snap);
    assert!(
        seen.contains(&a) && seen.contains(&b),
        "the full carries the view: {seen:?}"
    );
}
