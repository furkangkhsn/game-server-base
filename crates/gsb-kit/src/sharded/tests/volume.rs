//! The plain sharded room over the volumetric partition
//! (`GridPartition3`, BACKLOG A5), with the 3D fixture game (the third
//! axis is height): a climb across a horizontal seam migrates to the
//! shard above, a corner crossing relays over faces or — with the
//! diagonals — goes straight, routes are shortest in both
//! neighbourhoods, and the strip lends across a face (and, with the
//! diagonals, across an edge). The real actors are in `volume_actors`.

use crate::space::{GridPartition3, Partition};
use crate::testing::{Fixture3, Position3, WirePos3, WorldSnapshot3};

use super::*;

type Room3 = super::super::ShardedRoom<Fixture3, GridPartition3<Position3>>;

/// Shard `index` of a `shape` grid over `[-50, 50]³`.
fn shard(shape: [usize; 3], diagonals: bool, index: usize) -> Room3 {
    let grid = GridPartition3::new(shape, 50.0);
    let grid = if diagonals {
        grid.with_diagonals()
    } else {
        grid
    };
    Room3::with_game(Fixture3::default(), grid, index)
}

/// Join a player at `(x, y, z)`; its wire id.
fn place3(world: &mut World, room: &mut Room3, conn: u64, [x, y, z]: [f32; 3]) -> u64 {
    let admission = room.on_join(world, ConnectionId(conn));
    let entity = room.player_entity[&admission.player];
    world.entity_mut(entity).insert(Position3 { x, y, z });
    admission.entity
}

fn moved(world: &mut World, room: &Room3, player: u64, [x, y, z]: [f32; 3]) {
    let entity = room.player_entity[&PlayerId(player)];
    world.entity_mut(entity).insert(Position3 { x, y, z });
}

/// 2×2×2: shard 0 (x, y, z < 0) has the faces 1 (+x), 2 (+y), 4 (+z). A
/// climb through z = 0 is reported to shard 4 only, carries the height,
/// and installs there under the same wire id and player.
#[test]
fn a_climb_migrates_to_the_shard_above() {
    let (mut w0, mut w4) = (World::new(), World::new());
    let mut s0 = shard([2, 2, 2], false, 0);
    let mut s4 = shard([2, 2, 2], false, 4);
    assert_eq!(s0.neighbors(), &[1, 2, 4]);
    let wire = place3(&mut w0, &mut s0, 1, [-10.0, -10.0, -1.0]);
    assert!(s0.collect_migrations(&mut w0, 4).is_empty(), "not yet");

    moved(&mut w0, &s0, 1, [-10.0, -10.0, 0.0]); // z = 0 is the shard above's
    assert!(s0.collect_migrations(&mut w0, 1).is_empty());
    assert!(s0.collect_migrations(&mut w0, 2).is_empty());
    let up = s0.collect_migrations(&mut w0, 4);
    assert_eq!(up.len(), 1, "reported to the shard above once");
    let m = up.into_iter().next().expect("one");
    assert_eq!((m.wire, m.player), (wire, Some(PlayerId(1))));
    assert_eq!(m.state.game.z, 0.0, "the height travels");

    s4.on_migrate_in(&mut w4, m.wire, m.state, m.player);
    let mut q = w4.query::<(&WireId, &Position3)>();
    let arrived: Vec<_> = q.iter(&w4).map(|(w, p)| (w.get(), p.z)).collect();
    assert_eq!(arrived, vec![(wire, 0.0)]);
    assert!(s4.player_entity.contains_key(&PlayerId(1)));
}

/// A jump through the centre corner of 2×2×2, from region 0 to region 7:
/// with the faces only it relays (first hop: the first face, 1); with
/// the diagonals it goes straight to 7 in one hop.
#[test]
fn a_corner_crossing_relays_over_faces_or_goes_straight() {
    for (diagonals, hop) in [(false, 1), (true, 7)] {
        let mut w0 = World::new();
        let mut s0 = shard([2, 2, 2], diagonals, 0);
        let wire = place3(&mut w0, &mut s0, 1, [-1.0, -1.0, -1.0]);
        moved(&mut w0, &s0, 1, [1.0, 1.0, 1.0]); // region 7
        for n in s0.neighbors().to_vec() {
            let out = s0.collect_migrations(&mut w0, n);
            let expect: &[u64] = if n == hop { &[wire] } else { &[] };
            let got: Vec<u64> = out.iter().map(|m| m.wire).collect();
            assert_eq!(got, expect, "diagonals={diagonals}: to {n}");
        }
    }
}

/// Every route is a shortest path — in face steps (the Manhattan
/// distance between slots) by default, in king steps (the Chebyshev
/// distance) with the diagonals — each hop a neighbour.
#[test]
fn routes_are_shortest_in_both_neighbourhoods() {
    for shape in [[1, 1, 1], [2, 2, 2], [3, 3, 3], [4, 2, 3], [1, 3, 4]] {
        let [nx, ny, _] = shape;
        let slot = |i: usize| [i % nx, (i / nx) % ny, i / (nx * ny)];
        for diagonals in [false, true] {
            let grid = GridPartition3::<Position3>::new(shape, 50.0);
            let grid = if diagonals {
                grid.with_diagonals()
            } else {
                grid
            };
            let n = Partition::<WirePos3>::shard_count(&grid);
            let nb = |j: usize| Partition::<WirePos3>::neighbors(&grid, j);
            let tables: Vec<Vec<usize>> = (0..n).map(|i| first_hops(i, n, nb)).collect();
            for from in 0..n {
                for to in 0..n {
                    let (mut at, mut hops) = (from, 0);
                    while at != to {
                        let next = tables[at].get(to).copied().expect("a hop per region");
                        assert!(nb(at).contains(&next), "{shape:?}: {at}→{next}");
                        (at, hops) = (next, hops + 1);
                    }
                    let d = (0..3).map(|a| slot(from)[a].abs_diff(slot(to)[a]));
                    let shortest = if diagonals {
                        d.max().unwrap_or(0)
                    } else {
                        d.sum()
                    };
                    assert_eq!(hops, shortest, "{shape:?} {diagonals}: {from}→{to}");
                }
            }
        }
    }
}

/// 2×1×2 (x and height): shard 2 sits on shard 0. Shard 0 exports an
/// entity 1 below its ceiling and one 1 from its x = 0 wall but 30 below
/// the ceiling; shard 2's frame shows the first and not the second.
/// Shard 3 (across the edge x = 0, z = 0) hears shard 0 only with the
/// diagonals — and then shows an entity at that edge.
#[test]
fn the_strip_lends_across_a_horizontal_face_and_an_edge() {
    for diagonals in [false, true] {
        let mut w0 = World::new();
        let mut s0 = shard([2, 1, 2], diagonals, 0);
        let ceiling = place3(&mut w0, &mut s0, 1, [-30.0, 0.0, -1.0]);
        let wall = place3(&mut w0, &mut s0, 2, [-1.0, 0.0, -30.0]);
        let edge = place3(&mut w0, &mut s0, 3, [-1.0, 0.0, -1.0]);
        s0.update(&mut w0, &ctx(1));
        let strip = s0.collect_border(&w0);
        let exported: BTreeSet<u64> = strip.iter().map(|r| r.wire).collect();
        assert_eq!(exported, BTreeSet::from([ceiling, wall, edge]));

        let seen = |index: usize| {
            let (mut w, mut s) = (World::new(), shard([2, 1, 2], diagonals, index));
            let mut out = bytes::BytesMut::new();
            assert!(s.snapshot(&mut w, &ctx(1), &(), &strip, &mut out));
            let snap = WorldSnapshot3::decode(out.as_ref()).expect("snapshot");
            let ids: BTreeSet<u64> = snap.entities.iter().map(|e| e.entity).collect();
            (s.neighbors().contains(&0), ids)
        };
        let (hears, above) = seen(2);
        assert!(hears, "shard 2 is a face neighbour");
        assert_eq!(above, BTreeSet::from([ceiling, edge]), "{diagonals}");
        let (hears, across) = seen(3);
        assert_eq!(hears, diagonals, "the edge region hears shard 0");
        // The frame filter keeps the edge's corner of the strip; the core
        // hands shard 3 the strip only when it is a neighbour.
        assert_eq!(across, BTreeSet::from([edge]), "{diagonals}");
    }
}
