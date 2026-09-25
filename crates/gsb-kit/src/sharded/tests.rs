//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

use std::collections::BTreeSet;
use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::room::TickCtx;

use super::*;
use crate::identity::*;
use crate::space::Cell;
use crate::testing::*;
use gsb_core::id::PlayerId;
use gsb_core::room::GameLogic;
use gsb_core::shard::{BorderRecord, SHARD_SERIAL_RANGE, ShardLogic};
use prost::Message;

mod change_window;
mod crystal;
mod departing;
mod diagonals;
mod frame_filter;
mod ghosts;
mod input_carry;
mod lent_arrival;
mod migration;
mod seam;
mod spatial;
mod team;

/// The spatial composite over the same instantiation, with the kit's 2D
/// grid AOI.
type ShardedSpatialRoom = super::ShardedSpatialRoom<
    crate::testing::Fixture,
    crate::space::GridPartition2<Position>,
    crate::space::Grid2,
>;

/// The instantiation these tests drive: the fixture game over the kit's 2D
/// grid partition (shadows the generic room of `use super::*`).
type ShardedRoom =
    super::ShardedRoom<crate::testing::Fixture, crate::space::GridPartition2<Position>>;

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

/// Place a player at an exact position (join, then move the entity)
/// for a deterministic region assignment. Returns the wire id.
fn place(world: &mut World, room: &mut ShardedRoom, conn: ConnectionId, x: f32, y: f32) -> u64 {
    let admission = room.on_join(world, conn);
    let entity = *room
        .player_entity
        .get(&admission.player)
        .expect("registered");
    world.entity_mut(entity).insert(Position { x, y });
    admission.entity
}

fn snap_ids(out: &bytes::BytesMut) -> BTreeSet<u64> {
    crate::testing::WorldSnapshot::decode(out.as_ref())
        .expect("snapshot payload")
        .entities
        .iter()
        .map(|e| e.entity)
        .collect()
}

/// Region partition: every map point owns exactly one shard, the
/// regions tile the map (no gaps at the edges — clamping), and the
/// grid is balanced (rows*cols = N).
#[test]
fn region_partition_tiles_the_map() {
    for n in [1usize, 2, 3, 4, 6, 8, 12, 16] {
        let (rows, cols) = grid_shape(n);
        assert_eq!(rows * cols, n, "grid covers all shards (n={n})");
        let half = 50.0;
        // A grid of points across the whole map (and just outside it —
        // clamping keeps the "exactly one owner" invariant).
        for i in 0..=100 {
            for j in 0..=100 {
                let x = -60.0 + 120.0 * i as f32 / 100.0;
                let y = -60.0 + 120.0 * j as f32 / 100.0;
                let s = shard_at(x, y, half, n);
                assert!((0..n).contains(&s), "owner in range (n={n}): {s}");
            }
        }
        // Every shard owns at least one interior point.
        let mut owned = BTreeSet::new();
        for i in 0..=50 {
            for j in 0..=50 {
                let x = -50.0 + 100.0 * i as f32 / 50.0;
                let y = -50.0 + 100.0 * j as f32 / 50.0;
                owned.insert(shard_at(x, y, half, n));
            }
        }
        assert_eq!(owned.len(), n, "every shard has area (n={n})");
    }
}

/// Wire identity: the shards' ranges are disjoint and ids are stable
/// under migration (migrated-in keeps its id; the two shards' mints
/// never collide).
#[test]
fn wire_ranges_are_disjoint_and_stable() {
    let mut world0 = World::new();
    let mut world1 = World::new();
    let mut s0 = ShardedRoom::new(0, 4, 50.0);
    let mut s1 = ShardedRoom::new(1, 4, 50.0);

    let w0 = place(&mut world0, &mut s0, ConnectionId(1), -10.0, -10.0);
    let w1 = place(&mut world1, &mut s1, ConnectionId(2), 10.0, -10.0);
    assert!(w0 < SHARD_SERIAL_RANGE, "shard 0 in range 0: {w0}");
    assert!(
        (SHARD_SERIAL_RANGE..2 * SHARD_SERIAL_RANGE).contains(&w1),
        "shard 1 in range 1: {w1}"
    );
    assert_ne!(w0, w1, "disjoint ranges ⇒ no collision");

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
}

/// Migration: an entity crossing into a neighbor's region is reported
/// to EXACTLY that neighbor with its full state (position, speed,
/// target); `on_migrate_out` despawns it on the sender side.
#[test]
fn migration_reports_crossing_with_full_state() {
    let mut world = World::new();
    let mut s0 = ShardedRoom::new(0, 4, 50.0); // x in [-50, 0)
    let w = place(&mut world, &mut s0, ConnectionId(1), -1.0, -10.0);
    let entity = *s0.player_entity.get(&PlayerId(1)).unwrap();
    world
        .entity_mut(entity)
        .insert(MoveTarget { x: 1.0, y: -10.0 });

    // Still in shard 0: no migration to shard 1 (or anyone).
    assert!(
        s0.collect_migrations(&mut world, 1).is_empty(),
        "no crossing yet"
    );

    // Move across the seam (x: -1 → +1): now in shard 1's region.
    world
        .entity_mut(entity)
        .insert(Position { x: 1.0, y: -10.0 });
    let to1 = s0.collect_migrations(&mut world, 1);
    assert_eq!(to1.len(), 1, "crossing reported once");
    let m = &to1[0];
    assert_eq!(m.wire, w, "wire id carried");
    assert_eq!(m.state.pos.x, 1.0, "position carried");
    assert_eq!(m.state.target, Some(MoveTarget { x: 1.0, y: -10.0 }));
    assert_eq!(m.player, Some(PlayerId(1)), "stable player carried");
    // Not reported to the other neighbors.
    assert!(s0.collect_migrations(&mut world, 2).is_empty());
    assert!(s0.collect_migrations(&mut world, 3).is_empty());

    // On the sender side, migrate-out despawns and cleans the tables.
    s0.on_migrate_out(&mut world, w);
    assert!(world.get_entity(entity).is_err(), "despawned on sender");
    assert!(!s0.player_entity.contains_key(&PlayerId(1)));
    assert!(!s0.wire_entity.contains_key(&w));
}

/// Boundary visibility: an entity within the border margin of the
/// shared edge appears in the neighbor's snapshot (via the borrowed
/// records), so a player at the seam sees across it; an entity deep in
/// the shard (beyond the margin from the seam) does not. The export
/// covers entities near ANY edge of the shard — it is the consumer's
/// *frame filter* that decides what is actually visible (see the
/// `frame_filter_discards_far_neighbor_edges` test for that side).
#[test]
fn border_visibility_across_the_seam() {
    let half = 50.0;
    // 1 row × 2 cols: shard 0 = x in [-50,0], y in [-50,50]; shard 1 =
    // x in [0,50], y in [-50,50]. Border = min(cell_w,cell_h)/4 = 12.5.
    let mut w0 = World::new();
    let mut w1 = World::new();
    let mut s0 = ShardedRoom::new(0, 2, half);
    let mut s1 = ShardedRoom::new(1, 2, half);

    let near = place(&mut w0, &mut s0, ConnectionId(1), -1.0, -10.0); // 1 from the seam
    let far = place(&mut w0, &mut s0, ConnectionId(2), -40.0, -10.0); // 40 from the seam
    // Mirror the actor order: `update` (rebuilds the border cache)
    // before the border export.
    s0.update(&mut w0, &ctx(1));

    // Shard 1's snapshot (its own world is empty here) includes the
    // borrowed records that pass its frame filter.
    let borrowed: Vec<BorderRecord<WirePos>> = s0.collect_border(&w0);
    let mut out = bytes::BytesMut::new();
    assert!(
        s1.snapshot(&mut w1, &ctx(1), &(), &borrowed, &mut out),
        "neighbor emits with borrowed content"
    );
    let seen = snap_ids(&out);
    assert!(
        seen.contains(&near),
        "a player at the seam sees across it: {seen:?}"
    );
    assert!(
        !seen.contains(&far),
        "an entity 40 from the seam (beyond the 12.5 margin) is \
         invisible: {seen:?}"
    );
}

/// Frame filter: with a 4×4 grid, shard 0's EAST neighbor (shard 1)
/// exports its whole boundary; the parts of it near shard 1's OTHER
/// edges (far from shard 0) must not leak into shard 0's snapshots.
#[test]
fn frame_filter_discards_far_neighbor_edges() {
    // 4×4: cell = 25, border = 6.25. Shard 0: x,y in [-50,-25).
    // Shard 1 (east): x in [-25,0), y in [-50,-25).
    let half = 50.0;
    let mut w0 = World::new();
    let mut w1 = World::new();
    let mut s0 = ShardedRoom::new(0, 16, half);
    let mut s1 = ShardedRoom::new(1, 16, half);

    // An entity in shard 1 near its WEST edge (the seam with shard 0).
    let seam = place(&mut w1, &mut s1, ConnectionId(1), -24.0, -37.5);
    // An entity in shard 1 near its EAST edge (far from shard 0).
    let east = place(&mut w1, &mut s1, ConnectionId(2), -1.0, -37.5);

    // Mirror the actor order: `update` (rebuilds the border cache)
    // before the border export.
    s1.update(&mut w1, &ctx(1));

    let border = s1.collect_border(&w1);
    let wires: BTreeSet<u64> = border.iter().map(|r| r.wire).collect();
    assert!(
        wires.contains(&seam) && wires.contains(&east),
        "both are within shard 1's border frame: {wires:?}"
    );

    // Shard 0's snapshot (its own world is empty here — only the
    // borrowed content matters): the seam entity is in its frame, the
    // east entity is 23+ units away (beyond the 6.25 margin) and
    // filtered out.
    let borrowed: Vec<BorderRecord<WirePos>> = border;
    let mut out = bytes::BytesMut::new();
    assert!(s0.snapshot(&mut w0, &ctx(1), &(), &borrowed, &mut out));
    let seen = snap_ids(&out);
    assert!(seen.contains(&seam), "seam entity visible: {seen:?}");
    assert!(
        !seen.contains(&east),
        "far neighbor edge filtered: {seen:?}"
    );
}

/// The snapshot is a self-contained, order-stable union of own and
/// borrowed records; unchanged content (own + borrowed) stays silent.
#[test]
fn snapshot_union_and_no_change() {
    let mut w = World::new();
    let mut s = ShardedRoom::new(0, 4, 50.0);
    let a = place(&mut w, &mut s, ConnectionId(1), -10.0, -10.0);

    let borrowed = vec![BorderRecord {
        wire: 999,
        state: WirePos { x: -1, y: -10 },
    }];
    let mut out1 = bytes::BytesMut::new();
    assert!(s.snapshot(&mut w, &ctx(1), &(), &borrowed, &mut out1));
    let mut ids: Vec<u64> = snap_ids(&out1).into_iter().collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![a, 999], "own + borrowed union (sorted): {ids:?}");

    // Identical content (own + borrowed) ⇒ silent.
    let mut out2 = bytes::BytesMut::new();
    assert!(
        !s.snapshot(&mut w, &ctx(2), &(), &borrowed, &mut out2),
        "unchanged content ⇒ silent"
    );

    // A borrowed record moving is a content change ⇒ re-emit.
    let moved = vec![BorderRecord {
        wire: 999,
        state: WirePos { x: -1, y: -9 },
    }];
    let mut out3 = bytes::BytesMut::new();
    assert!(
        s.snapshot(&mut w, &ctx(3), &(), &moved, &mut out3),
        "borrowed movement ⇒ content change ⇒ emit"
    );
}
// ══ The Faz B spatial composite ([`ShardedSpatialRoom`]) ════════════
//
// The behavior locks of the `sharded × spatial` selection: cell
// grouping per shard, seam continuity through the borrowed strip, the
// evaporation guard (a static strip stays silent), and the
// fresh-member rule for migration arrivals. `cell_size = 20`,
// half = 50, 2 shards: s0 = x ∈ [-50, 0], s1 = x ∈ [0, 50]; border
// margin 12.5; wire x=-1 → Cell(-1, ·), x=5 → Cell(0, ·), x=25 →
// Cell(1, ·), x=45 → Cell(2, ·); y=-10 → row -1, y=15 → row 0.

/// Join a player on a [`ShardedSpatialRoom`] and move its entity to an
/// exact position. Returns the wire id.
fn place_spatial(
    world: &mut World,
    room: &mut ShardedSpatialRoom,
    conn: ConnectionId,
    x: f32,
    y: f32,
) -> u64 {
    let admission = room.on_join(world, conn);
    let entity = *room
        .inner
        .player_entity
        .get(&admission.player)
        .expect("registered");
    world.entity_mut(entity).insert(Position { x, y });
    admission.entity
}
