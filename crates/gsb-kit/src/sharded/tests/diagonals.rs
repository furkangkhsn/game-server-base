//! The grid preset's opt-in 8-neighbourhood
//! (`GridPartition2::with_diagonals`, KIT-ARCHITECTURE §10, F2): the
//! corner regions become neighbours, so the border strip is lent across
//! a corner and the migration routing takes the diagonal in one hop. The
//! default stays the 4-neighbourhood (its routes are pinned in
//! `migration.rs`).

use super::*;
use crate::space::{GridPartition2, Partition};

/// The fixture's partition of `n` shards over `[-50, 50]²`, 8-neighbourhood.
fn diagonal(n: usize) -> GridPartition2<Position> {
    GridPartition2::new(n, 50.0).with_diagonals()
}

fn neighbors(grid: &GridPartition2<Position>, idx: usize) -> Vec<usize> {
    Partition::<WirePos>::neighbors(grid, idx)
}

/// On a 2×2 grid shard 0 gains its diagonal (3) only when opted in; on a
/// 3×3 grid the centre has all eight regions, a corner three, an edge
/// five; the relation is symmetric on every grid shape.
#[test]
fn with_diagonals_adds_the_corner_regions() {
    let four = GridPartition2::<Position>::new(4, 50.0);
    assert_eq!(
        neighbors(&four, 0),
        vec![1, 2],
        "the default: 4-neighbourhood"
    );
    assert_eq!(neighbors(&diagonal(4), 0), vec![1, 2, 3]);
    assert_eq!(neighbors(&diagonal(4), 3), vec![2, 1, 0]);

    let nine = diagonal(9);
    assert_eq!(neighbors(&nine, 4), vec![3, 5, 1, 7, 0, 2, 6, 8]);
    assert_eq!(neighbors(&nine, 0), vec![1, 3, 4]);
    assert_eq!(neighbors(&nine, 1), vec![0, 2, 4, 3, 5]);

    for n in [1usize, 2, 3, 4, 6, 8, 12, 16] {
        let grid = diagonal(n);
        for a in 0..n {
            for b in neighbors(&grid, a) {
                assert!(neighbors(&grid, b).contains(&a), "n={n}: {a}↔{b}");
            }
        }
    }
}

/// With the 8-neighbourhood every route is a shortest path in KING moves
/// (the Chebyshev distance) — each hop a neighbour.
#[test]
fn routing_with_diagonals_takes_king_move_shortest_paths() {
    for n in [1usize, 2, 3, 4, 6, 8, 12, 16] {
        let (_, cols) = grid_shape(n);
        let grid = diagonal(n);
        let nb = |j: usize| neighbors(&grid, j);
        let tables: Vec<Vec<usize>> = (0..n).map(|i| first_hops(i, n, nb)).collect();
        for from in 0..n {
            for to in 0..n {
                let (mut at, mut hops) = (from, 0);
                while at != to {
                    let next = tables[at].get(to).copied().expect("a hop per region");
                    assert!(nb(at).contains(&next), "n={n}: hop {at}→{next}");
                    at = next;
                    hops += 1;
                }
                let king = (from / cols)
                    .abs_diff(to / cols)
                    .max((from % cols).abs_diff(to % cols));
                assert_eq!(hops, king, "n={n}: {from}→{to} is a shortest path");
            }
        }
    }
}

/// A crossing through the 2×2 grid's centre corner goes straight to the
/// diagonal shard — one hop, reported to it and to no edge neighbour.
#[test]
fn a_corner_crossing_goes_straight_to_the_diagonal_shard() {
    let mut w0 = World::new();
    let mut s0 = ShardedRoom::with_game(Fixture::default(), diagonal(4), 0);
    assert_eq!(s0.neighbors(), &[1, 2, 3]);
    let wire = place(&mut w0, &mut s0, ConnectionId(1), -1.0, -1.0);
    let entity = s0.player_entity[&PlayerId(1)];
    w0.entity_mut(entity).insert(Position { x: 1.0, y: 1.0 }); // region 3
    assert!(s0.collect_migrations(&mut w0, 1).is_empty());
    assert!(s0.collect_migrations(&mut w0, 2).is_empty());
    let to3 = s0.collect_migrations(&mut w0, 3);
    assert_eq!(to3.len(), 1, "one hop to the diagonal");
    assert_eq!((to3[0].wire, to3[0].player), (wire, Some(PlayerId(1))));
}

/// The diagonal shard lends across the corner: shard 0's entity 1 unit
/// from the centre corner shows on shard 3 (the corner square passes
/// its frame filter); one 1 unit from the seam but 40 along it does not.
#[test]
fn the_diagonal_shard_lends_across_the_corner() {
    let mut w0 = World::new();
    let mut w3 = World::new();
    let mut s0 = ShardedRoom::with_game(Fixture::default(), diagonal(4), 0);
    let mut s3 = ShardedRoom::with_game(Fixture::default(), diagonal(4), 3);
    assert!(s3.neighbors().contains(&0), "shard 0 sends its strip to 3");
    let corner = place(&mut w0, &mut s0, ConnectionId(1), -1.0, -1.0);
    let along = place(&mut w0, &mut s0, ConnectionId(2), -1.0, -40.0);
    s0.update(&mut w0, &ctx(1));
    let strip = s0.collect_border(&w0);
    let exported: BTreeSet<u64> = strip.iter().map(|r| r.wire).collect();
    assert_eq!(exported, BTreeSet::from([corner, along]));

    let mut out = bytes::BytesMut::new();
    assert!(s3.snapshot(&mut w3, &ctx(1), &(), &strip, &mut out));
    let seen = snap_ids(&out);
    assert!(seen.contains(&corner), "the corner is lent: {seen:?}");
    assert!(
        !seen.contains(&along),
        "far along the seam is not: {seen:?}"
    );
}
