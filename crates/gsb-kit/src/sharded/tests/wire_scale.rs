//! The partition presets' wire scale (BACKLOG A7) through the plain
//! sharded room: fixture games whose wire is in CENTIMETRES over metre
//! positions (`centi`). With `with_wire_scale(100.0)` a neighbour's strip
//! is admitted up to the margin, to the centimetre, and the room's unit
//! check passes; without it the frame filter drops the whole seam and a
//! debug build's check stops the room.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::space::{GridPartition2, GridPartition3};
use crate::testing::{Fixture3, WorldSnapshot3};

use super::*;

mod centi;

use centi::{Centi, CmCodec, CmCodec3, cm};

type Room2 = super::super::ShardedRoom<Centi<Fixture, CmCodec>, GridPartition2<Position>>;
type Room3 = super::super::ShardedRoom<Centi<Fixture3, CmCodec3>, GridPartition3<Position3>>;

/// Shard `index` of 2×2 over `[-512, 512]²` m (region 0: x, y < 0;
/// region 1 east of it; margin 128 m), at `scale` if one is given.
fn room2(index: usize, scale: Option<f32>) -> Room2 {
    let grid = GridPartition2::new(4, 512.0);
    let grid = match scale {
        Some(s) => grid.with_wire_scale(s),
        None => grid,
    };
    Room2::with_game(Centi::default(), grid, index)
}

/// Shard `index` of `[1, 2, 1]` over `[-64, 64]³` m (region 0 below
/// y = 0, region 1 above it; margin 16 m), at `scale` if one is given.
fn room3(index: usize, scale: Option<f32>) -> Room3 {
    let grid = GridPartition3::new([1, 2, 1], 64.0);
    let grid = match scale {
        Some(s) => grid.with_wire_scale(s),
        None => grid,
    };
    Room3::with_game(Centi::default(), grid, index)
}

fn place2(world: &mut World, room: &mut Room2, conn: u64, x: f32, y: f32) -> u64 {
    let admission = room.on_join(world, ConnectionId(conn));
    let entity = room.player_entity[&admission.player];
    world.entity_mut(entity).insert(Position { x, y });
    admission.entity
}

fn place3(world: &mut World, room: &mut Room3, conn: u64, [x, y, z]: [f32; 3]) -> u64 {
    let admission = room.on_join(world, ConnectionId(conn));
    let entity = room.player_entity[&admission.player];
    world.entity_mut(entity).insert(Position3 { x, y, z });
    admission.entity
}

/// What a fresh 2D shard `index` shows of `strip`: wire id → wire x.
fn shown2(index: usize, scale: Option<f32>, strip: &[BorderRecord<WirePos>]) -> BTreeMap<u64, i32> {
    let (mut w, mut s) = (World::new(), room2(index, scale));
    let mut out = bytes::BytesMut::new();
    if !s.snapshot(&mut w, &ctx(1), &(), strip, &mut out) {
        return BTreeMap::new();
    }
    let snap = WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    snap.entities.iter().map(|e| (e.entity, e.x)).collect()
}

/// What a fresh 3D shard `index` shows of `strip`: wire id → wire height.
fn shown3(
    index: usize,
    scale: Option<f32>,
    strip: &[BorderRecord<WirePos3>],
) -> BTreeMap<u64, i32> {
    let (mut w, mut s) = (World::new(), room3(index, scale));
    let mut out = bytes::BytesMut::new();
    if !s.snapshot(&mut w, &ctx(1), &(), strip, &mut out) {
        return BTreeMap::new();
    }
    let snap = WorldSnapshot3::decode(out.as_ref()).expect("snapshot");
    snap.entities.iter().map(|e| (e.entity, e.y)).collect()
}

/// Shard 1 lends an entity 100.25 m east of the seam, one 1 cm inside the
/// margin and its map edge (not the one deep inside); shard 0 shows the
/// first two in centimetres at the scale, nothing without it, and ends
/// its frame at 128 m to the centimetre.
#[test]
fn a_centimetre_strip_is_admitted_to_the_margin_at_the_scale() {
    let (mut w1, mut s1) = (World::new(), room2(1, Some(100.0)));
    let near = place2(&mut w1, &mut s1, 1, 100.25, -300.0);
    let edge = place2(&mut w1, &mut s1, 2, 127.99, -300.0);
    place2(&mut w1, &mut s1, 3, 300.0, -300.0);
    let east = place2(&mut w1, &mut s1, 4, 511.5, -300.0);
    s1.update(&mut w1, &ctx(1));
    let strip = s1.collect_border(&w1);
    let lent: BTreeSet<u64> = strip.iter().map(|r| r.wire).collect();
    assert_eq!(
        lent,
        BTreeSet::from([near, edge, east]),
        "export: positions"
    );

    let want = BTreeMap::from([(near, cm(100.25)), (edge, cm(127.99))]);
    assert_eq!(shown2(0, Some(100.0), &strip), want, "the seam, in cm");
    assert!(shown2(0, None, &strip).is_empty(), "unscaled: 100× farther");

    let rec = |n, x| BorderRecord {
        wire: interleaved_id(1, 4, n),
        state: WirePos { x, y: -30_000 },
    };
    let frame = shown2(0, Some(100.0), &[rec(20, 12_800), rec(21, 12_801)]);
    let ids: Vec<u64> = frame.into_keys().collect();
    assert_eq!(
        ids,
        vec![interleaved_id(1, 4, 20)],
        "128 m in, 128.01 m out"
    );
}

/// The volumetric room across the horizontal face: the upper shard shows
/// the lower one's entities 10.5 m and 15.99 m below its floor (not the
/// one by the cube's wall 30 m down), nothing without the scale, and
/// ends its frame 16 m down to the centimetre.
#[test]
fn a_centimetre_strip_is_admitted_across_a_face_at_the_scale() {
    let (mut w0, mut s0) = (World::new(), room3(0, Some(100.0)));
    let near = place3(&mut w0, &mut s0, 1, [0.5, -10.5, 0.5]);
    let edge = place3(&mut w0, &mut s0, 2, [0.5, -15.99, 0.5]);
    place3(&mut w0, &mut s0, 3, [0.5, -30.0, 0.5]);
    let wall = place3(&mut w0, &mut s0, 4, [63.5, -30.0, 0.5]);
    s0.update(&mut w0, &ctx(1));
    let strip = s0.collect_border(&w0);
    let lent: BTreeSet<u64> = strip.iter().map(|r| r.wire).collect();
    assert_eq!(
        lent,
        BTreeSet::from([near, edge, wall]),
        "export: positions"
    );

    let want = BTreeMap::from([(near, cm(-10.5)), (edge, cm(-15.99))]);
    assert_eq!(shown3(1, Some(100.0), &strip), want, "the face, in cm");
    assert!(shown3(1, None, &strip).is_empty(), "unscaled: 100× farther");

    let rec = |n, y| BorderRecord {
        wire: interleaved_id(0, 2, n),
        state: WirePos3 { x: 50, y, z: 50 },
    };
    let frame = shown3(1, Some(100.0), &[rec(20, -1_600), rec(21, -1_601)]);
    let ids: Vec<u64> = frame.into_keys().collect();
    assert_eq!(ids, vec![interleaved_id(0, 2, 20)], "16 m in, 16.01 m out");
}

/// The room checks every exported entity's wire against its position
/// (F3): at the scale the centimetre wire passes, without it a debug
/// build stops the room — in 2D and in 3D.
#[test]
fn the_rooms_unit_check_passes_at_the_scale_and_stops_without() {
    let runs2 = |scale| {
        let run = || {
            let (mut w, mut s) = (World::new(), room2(1, scale));
            place2(&mut w, &mut s, 1, 100.25, -300.0);
            s.update(&mut w, &ctx(1));
        };
        catch_unwind(AssertUnwindSafe(run)).is_ok()
    };
    let runs3 = |scale| {
        let run = || {
            let (mut w, mut s) = (World::new(), room3(0, scale));
            place3(&mut w, &mut s, 1, [0.5, -10.5, 0.5]);
            s.update(&mut w, &ctx(1));
        };
        catch_unwind(AssertUnwindSafe(run)).is_ok()
    };
    assert!(runs2(Some(100.0)) && runs3(Some(100.0)), "at the scale");
    if cfg!(debug_assertions) {
        assert!(!runs2(None), "2D: stopped without the scale");
        assert!(!runs3(None), "3D: stopped without the scale");
    }
}
