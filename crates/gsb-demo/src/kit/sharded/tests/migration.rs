//! Migration of entities that are not players (KIT-ARCHITECTURE §8.5):
//! every broadcast entity crosses a shard border with its owner region,
//! whatever components the game gave it.

use super::*;
use crate::kit::space::{GridPartition2, Partition};

/// An NPC the GAME spawned with a position and nothing else (no
/// `Speed`, no player) crosses from shard 0 into shard 1: it is reported
/// to shard 1 with its state, installed there under the SAME wire id
/// without components it never had, and despawned on shard 0.
#[test]
fn speedless_npc_migrates_across_the_seam() {
    let mut w0 = World::new();
    let mut w1 = World::new();
    let mut s0 = ShardedRoom::new(0, 4, 50.0); // 2×2: x∈[-50,0], y∈[-50,0]
    let mut s1 = ShardedRoom::new(1, 4, 50.0); // x∈[0,50], y∈[-50,0]

    let npc = w0.spawn(Position { x: -1.0, y: -10.0 }).id();
    s0.update(&mut w0, &ctx(1)); // stamps the NPC from shard 0's range
    let wire = w0.get::<WireId>(npc).expect("stamped").get();
    w0.entity_mut(npc).insert(Position { x: 1.0, y: -10.0 });

    let to1 = s0.collect_migrations(&mut w0, 1);
    assert_eq!(to1.len(), 1, "the NPC's crossing is reported to shard 1");
    let m = to1.into_iter().next().expect("one migration");
    assert_eq!(m.wire, wire, "the wire id travels");
    assert_eq!(m.player, None, "an NPC has no player");

    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    let mut q = w1.query::<(&WireId, &Position, Option<&Speed>)>();
    let arrived: Vec<_> = q
        .iter(&w1)
        .map(|(w, p, s)| (w.get(), *p, s.copied()))
        .collect();
    assert_eq!(
        arrived,
        vec![(wire, Position { x: 1.0, y: -10.0 }, None)],
        "installed on shard 1 as it was: same id, same position, still no Speed"
    );

    s0.on_migrate_out(&mut w0, wire);
    assert!(w0.get_entity(npc).is_err(), "despawned on shard 0");
}

/// A crossing into a region that is NOT a neighbour of the owner (the
/// 2×2 grid's diagonal: a move through the corner where four shards
/// meet, or any jump): the entity is handed to the neighbour on a
/// shortest path toward its region — exactly one neighbour — and that
/// shard hands it on to the owner (§8.4: before, it was reported to
/// nobody and stayed with the wrong owner).
#[test]
fn non_adjacent_crossing_is_routed_through_a_neighbour() {
    let mut w0 = World::new();
    let mut w1 = World::new();
    // 2×2: shard 0 = x<0,y<0; 1 = x≥0,y<0; 2 = x<0,y≥0; 3 = x≥0,y≥0.
    let mut s0 = ShardedRoom::new(0, 4, 50.0);
    let mut s1 = ShardedRoom::new(1, 4, 50.0);
    let wire = place(&mut w0, &mut s0, ConnectionId(1), -1.0, -1.0);
    let entity = s0.player_entity[&PlayerId(1)];
    w0.entity_mut(entity).insert(Position { x: 1.0, y: 1.0 }); // region 3

    let to1 = s0.collect_migrations(&mut w0, 1);
    let to2 = s0.collect_migrations(&mut w0, 2);
    assert_eq!(
        to1.len() + to2.len(),
        1,
        "handed to exactly one neighbour (to 1: {}, to 2: {})",
        to1.len(),
        to2.len()
    );
    // The grid routes columns first: shard 1 is the first hop.
    let m = to1.into_iter().next().expect("routed through shard 1");
    assert_eq!((m.wire, m.player), (wire, Some(PlayerId(1))));

    // The intermediate shard installs it and hands it on to the owner.
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    let to3 = s1.collect_migrations(&mut w1, 3);
    assert_eq!(to3.len(), 1, "the second hop reaches region 3's owner");
    assert_eq!((to3[0].wire, to3[0].player), (wire, Some(PlayerId(1))));
    assert!(
        s1.collect_migrations(&mut w1, 0).is_empty(),
        "never sent back the way it came"
    );
}

/// The routing table on every grid shape: from any shard, every other
/// region's first hop is a neighbour, and following the hops reaches the
/// region along a shortest (Manhattan) path.
#[test]
fn routing_reaches_every_region_by_shortest_paths() {
    for n in [1usize, 2, 3, 4, 6, 8, 12, 16] {
        let (_, cols) = grid_shape(n);
        let grid = GridPartition2::<Position>::new(n, 50.0);
        let neighbors = |j: usize| Partition::<WirePos>::neighbors(&grid, j);
        let tables: Vec<Vec<usize>> = (0..n).map(|i| first_hops(i, n, neighbors)).collect();
        for from in 0..n {
            for to in 0..n {
                let (mut at, mut hops) = (from, 0);
                while at != to {
                    let next = tables[at].get(to).copied().expect("a hop per region");
                    assert!(
                        neighbors(at).contains(&next),
                        "n={n}: hop {at}→{next} toward {to} is a neighbour"
                    );
                    at = next;
                    hops += 1;
                }
                let manhattan =
                    (from / cols).abs_diff(to / cols) + (from % cols).abs_diff(to % cols);
                assert_eq!(hops, manhattan, "n={n}: {from}→{to} is a shortest path");
            }
        }
    }
}

/// An NPC leaving a spatial shard by migration removes its record but
/// no membership: the cell it shared with a player keeps that player's
/// member count (a migrating NPC used to be booked out as a member,
/// zeroing the count — the next arrival would then fake a group birth).
#[test]
fn npc_migrating_out_keeps_the_cells_member_count() {
    let mut world = World::new();
    let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
    let _p = place_spatial(&mut world, &mut s1, ConnectionId(1), 5.0, -10.0); // Cell(0,-1)
    let npc = world.spawn(Position { x: 8.0, y: -8.0 }).id(); // Cell(0,-1)
    s1.update(&mut world, &ctx(1));
    let npc_wire = world.get::<WireId>(npc).expect("stamped").get();
    assert_eq!(s1.book.member_counts.get(&Cell(0, -1)), Some(&1));

    s1.on_migrate_out(&mut world, npc_wire);
    s1.update(&mut world, &ctx(2));
    assert_eq!(
        s1.book.member_counts.get(&Cell(0, -1)),
        Some(&1),
        "the player is still the cell's one member"
    );
}
