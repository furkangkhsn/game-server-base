//! Despawns the shard did not cause (KIT-ARCHITECTURE §8.2): an entity
//! the GAME despawns must leave the spatial composite's views and the
//! shard's own tables, like a leaver or a migrant does.

use super::*;
use crate::kit::space::{Grid2, GridPartition2};
use crate::kit::testing::{Culling, Doomed};

/// An NPC beside an observer on shard 1, despawned by game code on tick
/// 2: the observer's group gets a delta removing it, and the shard no
/// longer lists its wire id among its own (the core's duplicate filter
/// and the migration table read that set).
#[test]
fn an_npc_despawned_by_game_code_leaves_the_shard() {
    let mut world = World::new();
    let mut s1 = super::super::ShardedSpatialRoom::with_shard(
        super::super::ShardedRoom::with_game(
            Culling(Fixture::default()),
            GridPartition2::<Position>::new(2, 50.0),
            1,
        ),
        Grid2::new(20.0),
    );
    let observer = s1.on_join(&mut world, ConnectionId(1));
    let entity = s1.inner.player_entity[&observer.player];
    world
        .entity_mut(entity)
        .insert(Position { x: 5.0, y: -10.0 }); // Cell(0,-1)
    let npc = world.spawn(Position { x: 8.0, y: -8.0 }).id(); // Cell(0,-1)
    s1.update(&mut world, &ctx(1));
    let npc_wire = world.get::<WireId>(npc).expect("stamped").get();
    let mut out = bytes::BytesMut::new();
    assert!(s1.snapshot(&mut world, &ctx(1), &Cell(0, -1), &[], &mut out));

    world.entity_mut(npc).insert(Doomed);
    s1.update(&mut world, &ctx(2));
    assert!(world.get_entity(npc).is_err(), "the game despawned it");

    let mut out = bytes::BytesMut::new();
    assert!(
        s1.snapshot(&mut world, &ctx(2), &Cell(0, -1), &[], &mut out),
        "the despawn is news for the group"
    );
    let delta = WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    assert_eq!(delta.removed, vec![npc_wire], "the NPC is removed");
    assert!(
        !s1.own_wires(&world).contains(&npc_wire),
        "the shard no longer owns a dead wire id"
    );
    assert!(!s1.inner.wire_entity.contains_key(&npc_wire));
}
