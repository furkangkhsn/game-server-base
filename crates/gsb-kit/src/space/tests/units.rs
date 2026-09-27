//! The unit contract of [`Planar`] (KIT-ARCHITECTURE §10, F3): a preset
//! reading both a position and a wire value — [`GridPartition2`] —
//! compares them in ONE unit, so the two projections must report the
//! same one. The symptom of a mismatch, and the debug-build check that
//! catches it (in the preset and through the sharded room).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// The 3D game's wire quantized to DECIMETRES and — the mistake —
/// projected in decimetres, while its position is in metres.
struct WireDm {
    x: i32,
    z: i32,
}

impl Planar for WireDm {
    type Coord = i32;
    fn planar(&self) -> [i32; 2] {
        [self.x, self.z]
    }
}

/// 2×2 over `[-512, 512]²` metres: region 0 is x < 0, z < 0; the border
/// margin is 128 m.
fn grid() -> GridPartition2<Pos3> {
    GridPartition2::new(4, 512.0)
}

/// The symptom: shard 0's frame filter must keep region 1's strip within
/// 128 m of the x = 0 seam (the records sit 50 m south of the map's
/// centre line, inside region 0's rows in either unit). With the wire projected in metres it does;
/// in decimetres the same records read ten times farther out, and the
/// margin shrinks to 12.8 m — an entity 20 m across the seam is dropped.
#[test]
fn a_wire_projected_in_a_finer_unit_shrinks_the_frame_filter() {
    let g = grid();
    let admits_m = |x: i32| Partition::<Wire3>::admits(&g, 0, &wire(x, 0, -50));
    let admits_dm = |x: i32| Partition::<WireDm>::admits(&g, 0, &WireDm { x, z: -500 });
    assert!(admits_m(20) && admits_m(120), "metres: the 128 m margin");
    assert!(!admits_m(140), "metres: beyond the margin");
    assert!(admits_dm(100), "decimetres: 10 m still passes…");
    assert!(!admits_dm(200), "…but 20 m across the seam is dropped");
}

/// A wire in the position's unit passes the check wherever the entity
/// is — truncated or rounded, inside the map or strayed outside it.
#[test]
fn the_debug_check_accepts_a_wire_in_the_positions_unit() {
    let g = grid();
    for x in [-700.0f32, -511.6, -0.4, 0.0, 0.6, 127.9, 511.5, 900.2] {
        for z in [-600.3f32, -1.5, 0.0, 300.7] {
            let p = pos(x, 50.0, z);
            Partition::<Wire3>::debug_check_wire(&g, &p, &wire(x as i32, 0, z.round() as i32));
        }
    }
}

/// The same mistake as the symptom test, caught in a debug build.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "must report the position's unit")]
fn the_debug_check_catches_a_wire_in_a_finer_unit() {
    let wire = WireDm { x: 200, z: -1_000 };
    Partition::<WireDm>::debug_check_wire(&grid(), &pos(20.0, 0.0, -100.0), &wire);
}

/// A partition that counts the checks it is asked for (and delegates
/// everything to the fixture's grid).
struct Probe(GridPartition2<crate::testing::Position>, Arc<AtomicUsize>);

impl Partition<crate::testing::WirePos> for Probe {
    type Pos = crate::testing::Position;
    fn shard_count(&self) -> usize {
        Partition::<crate::testing::WirePos>::shard_count(&self.0)
    }
    fn region_of(&self, pos: &Self::Pos) -> usize {
        Partition::<crate::testing::WirePos>::region_of(&self.0, pos)
    }
    fn neighbors(&self, idx: usize) -> Vec<usize> {
        Partition::<crate::testing::WirePos>::neighbors(&self.0, idx)
    }
    fn exports(&self, idx: usize, pos: &Self::Pos) -> bool {
        Partition::<crate::testing::WirePos>::exports(&self.0, idx, pos)
    }
    fn admits(&self, idx: usize, wire: &crate::testing::WirePos) -> bool {
        self.0.admits(idx, wire)
    }
    fn debug_check_wire(&self, pos: &Self::Pos, wire: &crate::testing::WirePos) {
        self.1.fetch_add(1, Ordering::Relaxed);
        self.0.debug_check_wire(pos, wire);
    }
}

/// The sharded room asks for the check once per EXPORTED entity each
/// time it rebuilds its strip — the entity 1 unit from the seam, not
/// the one deep in the region.
#[test]
fn the_sharded_room_checks_every_exported_entity() {
    use crate::testing::{Fixture, Position};
    use gsb_core::id::{ConnectionId, RoomId};
    use gsb_core::room::{GameLogic, TickCtx};

    let checks = Arc::new(AtomicUsize::new(0));
    let probe = Probe(GridPartition2::new(2, 50.0), checks.clone());
    let mut room = crate::sharded::ShardedRoom::with_game(Fixture::default(), probe, 0);
    let mut world = bevy_ecs::world::World::new();
    for (conn, x) in [(1, -1.0), (2, -25.0)] {
        room.on_join(&mut world, ConnectionId(conn));
        let mut q = world.query::<&mut Position>();
        let mut placed = q.iter_mut(&mut world).filter(|p| p.x == 0.0);
        *placed.next().expect("the new player") = Position { x, y: 0.0 };
    }
    let ctx = TickCtx {
        room: RoomId(1),
        tick: 1,
        dt: std::time::Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: Default::default(),
    };
    room.update(&mut world, &ctx);
    assert_eq!(checks.load(Ordering::Relaxed), 1, "one exported entity");
}
