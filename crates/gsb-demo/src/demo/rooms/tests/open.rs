//! The open room over the demo game: a plain position write and an
//! orphan's stamp, read back as the demo's truncated record.

use super::*;
use crate::room::OpenRoom;

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
    let e = entity_of(&mut world, wire_id);
    world.entity_mut(e).insert(Position { x: 42.0, y: -7.0 });

    let mut out2 = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx, &(), &[], &mut out2),
        "a plain position write must still emit"
    );
    let snap = WorldSnapshot::decode(out2.as_ref()).expect("decode");
    assert_eq!(snap.entities.len(), 1);
    assert_eq!(snap.entities[0].x, 42);
    assert_eq!(snap.entities[0].y, -7);
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
    let snap = WorldSnapshot::decode(out.as_ref()).expect("decode");
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
    let snap3 = WorldSnapshot::decode(out3.as_ref()).expect("decode");
    let rec3 = snap3
        .entities
        .iter()
        .find(|e| e.entity == 3)
        .expect("same identity in the new snapshot");
    assert_eq!((rec3.x, rec3.y), (9, -3));
}
