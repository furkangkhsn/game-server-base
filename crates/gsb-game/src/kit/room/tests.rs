//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

use super::*;
use std::time::Duration;

use crate::kit::identity::*;
use crate::kit::seam::*;
use bevy_ecs::prelude::*;
use gsb_core::id::RoomId;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{GameLogic, TickCtx};
use prost::Message;

fn ctx1() -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick: 1,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

/// The "no change" decision compares the wire content: a plain
/// `Position` write — no version component, no bump discipline — must
/// still be broadcast whenever it changes a truncated coordinate.
#[test]
fn snapshot_emits_on_plain_position_write() {
    let mut world = World::new();
    let mut room = OpenRoom::new();
    let wire_id = room.on_join(&mut world, ConnectionId(1)).entity;
    let ctx = ctx1();
    let mut out = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx, &(), &[], &mut out),
        "join emits"
    );
    assert_eq!(wire_id, 1, "first entity gets wire id 1");

    // The bevy handle is the room's business (player_entity); the
    // join reply carried the wire id, not the bevy bits.
    let e = *room.player_entity.get(&PlayerId(1)).unwrap();
    world.entity_mut(e).insert(Position { x: 42.0, y: -7.0 });

    let mut out2 = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx, &(), &[], &mut out2),
        "a plain position write must still emit"
    );
    let snap = crate::kit::seam::WorldSnapshot::decode(out2.as_ref()).expect("decode");
    assert_eq!(snap.entities.len(), 1);
    assert_eq!(snap.entities[0].x, 42);
    assert_eq!(snap.entities[0].y, -7);
}

/// The identity invariant (see `game.proto`, `EntityRecord.entity`):
/// the client's world view is its **last accepted** snapshot, and an
/// identity present in both the old and the new snapshot must be the
/// *same* entity — that is what separates "the same entity moved"
/// from "a new entity took the slot" when there is no delta, no
/// history, and no out-of-band remapping message.
///
/// The threat this test pins: the bevy allocator **recycles slots**
/// (a despawned entity's index is handed back out with a bumped
/// generation). In bevy 0.19 the allocator keeps freed indices in a
/// local buffer of 128 before they become reusable, so this test
/// runs 129 join/leave cycles to force a reuse, then asserts:
///
/// 1. the reuse actually happened (the next join lands on an index a
///    previous entity owned) — the test is not vacuous; an
///    index-based wire identity would be *indistinguishable* here;
/// 2. the recycled slot carries a **fresh** wire id never seen
///    before — so a client whose accepted view still contains the
///    old entity (it lost the leave snapshots) reads the new
///    snapshot as "new entity", not "old entity moved".
#[test]
fn wire_identity_survives_ecs_slot_reuse() {
    let mut world = World::new();
    let mut room = OpenRoom::new();
    let ctx = ctx1();

    // 129 join/leave cycles: every join gets a fresh wire id, every
    // leave despawns the entity (freeing its bevy slot).
    let mut wire_ids: Vec<u64> = Vec::new();
    let (mut index_128, mut gen_128) = (None, 0u32);
    for i in 1..=129u64 {
        let conn = ConnectionId(i);
        let wire_id = room.on_join(&mut world, conn).entity;
        assert!(
            !wire_ids.contains(&wire_id),
            "wire id {wire_id} handed out twice"
        );
        wire_ids.push(wire_id);
        // The n-th sequential join owns PlayerId(n) (the counter).
        let e = *room.player_entity.get(&PlayerId(i)).unwrap();
        if i == 128 {
            index_128 = Some(e.index_u32());
            gen_128 = e.generation().to_bits();
        }
        room.on_leave(&mut world, PlayerId(i));
    }

    // The next join must recycle a bevy slot (129 frees overflow the
    // 128-slot local free buffer): exactly the condition under which
    // a non-unique wire identity would break the invariant.
    let rejoiner = ConnectionId(1000);
    let wire_id = room.on_join(&mut world, rejoiner).entity;
    // The 130th sequential join owns PlayerId(130).
    let e = *room.player_entity.get(&PlayerId(130)).unwrap();
    assert_eq!(
        e.index_u32(),
        index_128.expect("recorded above"),
        "bevy slot reuse must have happened for this test to be \
         non-vacuous (an index-based identity would alias here)"
    );
    assert_ne!(
        e.generation().to_bits(),
        gen_128,
        "recycled slot carries a bumped generation"
    );
    assert!(
        !wire_ids.contains(&wire_id),
        "recycled slot must carry a fresh wire id, never a previous one"
    );

    // Client model from `game.proto`: the client's accepted view still
    // holds entity #128 (it lost the leave snapshots). From the new
    // snapshot alone it must classify the record.
    let mut out = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx, &(), &[], &mut out),
        "join emits"
    );
    let snap = crate::kit::seam::WorldSnapshot::decode(out.as_ref()).expect("decode");
    assert_eq!(snap.entities.len(), 1);
    let rec = &snap.entities[0];
    assert_eq!(rec.entity, wire_id, "snapshot carries the fresh wire id");
    let old_view: std::collections::HashSet<u64> = [wire_ids[127]].into_iter().collect();
    assert!(
        !old_view.contains(&rec.entity),
        "the client must see a NEW entity, not entity #128 moving \
         (its old wire id is gone; the recycled slot's new id was \
         never in the client's view)"
    );
    // Note what an index-based identity would have produced here: the
    // record's bevy index equals entity #128's index (asserted above),
    // so a client keyed by index would hit its map and misread the new
    // entity as entity #128 teleporting to a spawn point. The wire id
    // is the field that carries the distinction.
}

/// The publishable precondition is structural, not a discipline: an
/// entity that carries a [`Position`] but never passed through
/// [`OpenRoom::on_join`] (bullets, NPCs, traps — anything not
/// player-spawned) must not be *silently invisible*. The broadcast
/// pass stamps it with a fresh serial and includes it in the very
/// next snapshot — restoring the pre-compact-identity contract
/// (broadcast set = "has a `Position`").
#[test]
fn entity_spawned_outside_on_join_is_broadcast_with_fresh_wire_id() {
    let mut world = World::new();
    let mut room = OpenRoom::new();
    let ctx = ctx1();

    // Two players through the normal path (wire ids 1 and 2).
    room.on_join(&mut world, ConnectionId(1));
    room.on_join(&mut world, ConnectionId(2));

    // A "bullet" spawned directly into the world — no `on_join`.
    let bullet = world.spawn(Position { x: 7.0, y: -3.0 }).id();
    assert!(
        world.get::<WireId>(bullet).is_none(),
        "precondition: the entity has no wire identity"
    );

    // The next snapshot must include it, with a fresh wire id.
    let mut out = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx, &(), &[], &mut out),
        "a new entity is a wire-content change ⇒ emit"
    );
    let snap = crate::kit::seam::WorldSnapshot::decode(out.as_ref()).expect("decode");
    assert_eq!(
        snap.entities.len(),
        3,
        "the orphan must not be silently invisible"
    );
    let rec = snap
        .entities
        .iter()
        .find(|e| e.x == 7 && e.y == -3)
        .expect("the orphan's record");
    assert_eq!(
        rec.entity, 3,
        "it gets the next free serial from the room's single counter \
         (fresh: never handed out before, never re-used)"
    );
    assert!(
        world.get::<WireId>(bullet).is_some(),
        "the entity is stamped (one assignment)"
    );

    // Idempotent: the same wire content emits nothing, and the
    // identity is stable across snapshots.
    let mut out2 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx, &(), &[], &mut out2),
        "unchanged content ⇒ silent (no re-stamp, no re-emit)"
    );
    assert_eq!(
        world.get::<WireId>(bullet).copied().map(WireId::get),
        Some(3),
        "the identity is stable across snapshots"
    );

    // The stamped entity moves: the *same* identity at a new position
    // (a client reads "the same entity moved", not "a new entity").
    world
        .entity_mut(bullet)
        .insert(Position { x: 9.0, y: -3.0 });
    let mut out3 = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx, &(), &[], &mut out3),
        "movement ⇒ wire content changed ⇒ emit"
    );
    let snap3 = crate::kit::seam::WorldSnapshot::decode(out3.as_ref()).expect("decode");
    let rec3 = snap3
        .entities
        .iter()
        .find(|e| e.entity == 3)
        .expect("same identity in the new snapshot");
    assert_eq!((rec3.x, rec3.y), (9, -3));
}

/// Identical wire content stays silent (a write that leaves the
/// wire content untouched emits nothing); a membership change is a
/// wire-content change and must emit.
#[test]
fn snapshot_silent_when_wire_content_unchanged() {
    let mut world = World::new();
    let mut room = OpenRoom::new();
    room.on_join(&mut world, ConnectionId(1));
    let ctx = ctx1();
    let mut out = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx, &(), &[], &mut out),
        "join emits"
    );

    // A position write with no content change...
    let entity = *room.player_entity.get(&PlayerId(1)).unwrap();
    let pos = world
        .entity(entity)
        .get::<Position>()
        .copied()
        .expect("spawned above");
    world.entity_mut(entity).insert(pos);
    let mut out2 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx, &(), &[], &mut out2),
        "write without content change ⇒ no change ⇒ silent"
    );

    // ...and a leave is a wire-content change.
    room.on_leave(&mut world, PlayerId(1));
    let mut out3 = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx, &(), &[], &mut out3),
        "leave ⇒ wire content changed ⇒ emit"
    );
    let snap = crate::kit::seam::WorldSnapshot::decode(out3.as_ref()).expect("decode");
    assert!(snap.entities.is_empty(), "left: empty world snapshot");
}
