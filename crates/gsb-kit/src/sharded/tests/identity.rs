//! Wire identity (module docs, "Wire identity"): the shards of a room
//! draw interleaved values — compact, unique across shards without
//! coordination, never drawn twice, and kept across a migration.

use std::collections::HashSet;

use gsb_core::shard::minting_shard;

use super::*;

/// Each shard's first join draws its first two values (player, then
/// entity): small ones of its own residue class. The ids never collide,
/// and a migrated entity keeps its id on the receiving shard.
#[test]
fn wire_ids_are_compact_disjoint_and_stable() {
    let mut world0 = World::new();
    let mut world1 = World::new();
    let mut s0 = ShardedRoom::new(0, 4, 50.0);
    let mut s1 = ShardedRoom::new(1, 4, 50.0);

    let w0 = place(&mut world0, &mut s0, ConnectionId(1), -10.0, -10.0);
    let w1 = place(&mut world1, &mut s1, ConnectionId(2), 10.0, -10.0);
    assert_eq!((w0, w1), (5, 6), "each shard's second draw (N = 4)");
    assert_eq!((minting_shard(w0, 4), minting_shard(w1, 4)), (0, 1));

    // Migrate w0 from shard 0 into shard 1: the id is preserved.
    let entity0 = *s0.player_entity.get(&PlayerId(1)).unwrap();
    let state = KitMig {
        game: FixMig {
            pos: world0.entity(entity0).get::<Position>().copied().unwrap(),
            speed: world0.entity(entity0).get::<Speed>().map(|s| s.0),
            target: world0.entity(entity0).get::<MoveTarget>().copied(),
        },
        park: None,
        input: None,
        pin: None,
    };
    s1.on_migrate_in(&mut world1, w0, state, Some(PlayerId(1)));
    let entity1 = *s1.player_entity.get(&PlayerId(1)).unwrap();
    assert_eq!(
        world1.entity(entity1).get::<WireId>().unwrap().get(),
        w0,
        "the migrated entity keeps its wire id"
    );
    // The arrival is not a draw: shard 1 goes on in its own class.
    let next = place(&mut world1, &mut s1, ConnectionId(3), 20.0, -10.0);
    assert_eq!(next, 14, "shard 1's fourth draw");
    assert_eq!(s1.serial_used(), 4);
}

/// Churn on all four shards of a room — a join, a game-spawned NPC, the
/// NPC's despawn and the player's leave, fifty rounds each — never
/// draws a value twice (player ids and wire ids share the counter): the
/// 600 draws are exactly the values 1..=600, each naming its shard.
#[test]
fn a_value_is_never_drawn_twice_in_a_room() {
    let mut worlds: Vec<World> = (0..4).map(|_| World::new()).collect();
    let mut shards: Vec<ShardedRoom> = (0..4).map(|i| ShardedRoom::new(i, 4, 50.0)).collect();
    let mut drawn = HashSet::new();
    let mut conn = 0;
    for round in 1..=50 {
        for (i, (w, s)) in worlds.iter_mut().zip(shards.iter_mut()).enumerate() {
            conn += 1;
            let admission = s.on_join(w, ConnectionId(conn));
            let npc = w.spawn(Position { x: 0.0, y: 0.0 }).id();
            s.update(w, &ctx(round));
            let stamped = w.get::<WireId>(npc).expect("stamped").get();
            for v in [admission.player.0, admission.entity, stamped] {
                assert!(drawn.insert(v), "{v} drawn twice");
                assert_eq!(minting_shard(v, 4), i, "{v} names its shard");
            }
            w.despawn(npc);
            s.on_leave(w, admission.player);
            s.update(w, &ctx(round));
        }
    }
    assert_eq!(drawn.len(), 600);
    assert_eq!(drawn.iter().max(), Some(&600), "dense: 1..=600");
    assert!(shards.iter().all(|s| s.serial_used() == 150));
    assert!(
        shards
            .iter()
            .all(|s| s.serial_capacity() == gsb_core::shard::SHARD_SERIAL_CAPACITY)
    );
}
