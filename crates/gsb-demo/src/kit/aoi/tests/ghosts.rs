//! Despawns the room did not cause (KIT-ARCHITECTURE §8.2): an entity
//! the GAME despawns — an NPC dying in the game's systems — must leave
//! every view it was in, like a leaver does.

use super::*;
use crate::kit::testing::{Culling, Doomed};

/// An NPC in the observer's cell, despawned by game code on tick 2: the
/// group's tick-2 packet is a delta removing it, the keep-alive full no
/// longer carries it, and the cell book has forgotten it (no ghost left
/// for any later packet to resurrect).
#[test]
fn an_npc_despawned_by_game_code_leaves_every_view() {
    let mut world = World::new();
    let mut room = super::super::AoiRoom::with_game(
        Culling(crate::kit::testing::Fixture::default()),
        crate::kit::space::Grid2::new(20.0),
    );
    let observer = room.on_join(&mut world, ConnectionId(1));
    let entity = room.player_entity[&observer.player];
    world.entity_mut(entity).insert(Position { x: 0.0, y: 0.0 }); // Cell(0,0)
    let npc = world
        .spawn((Position { x: 5.0, y: 5.0 }, Speed(DEFAULT_SPEED)))
        .id(); // Cell(0,0)
    room.update(&mut world, &ctx(1));
    let npc_wire = world.get::<WireId>(npc).expect("stamped").get();
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &[], &mut out));
    let full = WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    assert!(
        full.entities.iter().any(|e| e.entity == npc_wire),
        "baseline"
    );

    world.entity_mut(npc).insert(Doomed);
    room.update(&mut world, &ctx(2));
    assert!(world.get_entity(npc).is_err(), "the game despawned it");

    let mut out = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &[], &mut out),
        "the despawn is news for the group"
    );
    let delta = WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    assert!(delta.delta, "an established group gets a delta");
    assert_eq!(delta.removed, vec![npc_wire], "the NPC is removed");

    let mut out = bytes::BytesMut::new();
    assert!(room.keepalive(&mut world, &ctx(2), &Cell(0, 0), None, &mut out));
    let ids: Vec<u64> = WorldSnapshot::decode(out.as_ref())
        .expect("snapshot")
        .entities
        .iter()
        .map(|e| e.entity)
        .collect();
    assert_eq!(ids, vec![observer.entity], "no ghost in the full view");
    assert!(
        !room.book.last_cell.contains_key(&npc),
        "the book forgot the entity"
    );
}
