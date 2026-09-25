use bytes::BytesMut;
use gsb_kit::space::Partition;
use prost::Message;

use super::*;
use crate::world::{WORLD_HALF, partition};

const SAMPLES: [i32; 8] = [0, 1, -1, 9, -9, 8_000, -8_000, i32::MIN];

fn wire(x: i32, y: i32, z: i32, faction: u8) -> WarWire {
    WarWire {
        x,
        y,
        z,
        kind: Kind::Player,
        faction,
        hp: 75,
    }
}

/// Rounded to the nearest decimetre, symmetric around zero, saturating
/// far outside the world.
#[test]
fn quantization_rounds_to_the_nearest_decimetre() {
    let unit = Unit {
        kind: Kind::Tower,
        faction: Some(Team(2)),
        hp: 0,
    };
    let q = WarWire::of(&Pos3::new(1.26, 12.0, -0.04), &unit);
    assert_eq!((q.x, q.y, q.z, q.faction), (13, 120, 0, 3));
    assert_eq!(
        (to_dm(-0.06), to_dm(0.05), to_dm(-WORLD_HALF)),
        (-1, 1, -8_000)
    );
    assert_eq!(to_dm(1.0e12), i32::MAX);
    assert_eq!(q.pos(), Pos3::new(1.3, 12.0, 0.0));
}

/// The wire numbers factions from 1; 0 is none, and anything that is no
/// faction reads as none.
#[test]
fn wire_factions_are_one_based() {
    assert_eq!(wire_faction(None), 0);
    for t in 0..3u8 {
        assert_eq!(wire_faction(Some(Team(t))), t + 1);
        assert_eq!(team_of_wire(u32::from(t) + 1), Some(Team(t)));
    }
    assert_eq!(team_of_wire(0), None);
    assert_eq!(team_of_wire(1 << 20), None);
}

/// The codec's record body is exactly the typed `UnitRecord`; a unit on
/// the ground of faction none writes neither field.
#[test]
fn record_body_is_the_unit_record_encoding() {
    for id in [1u64, 127, 128, 3 << 20, u64::MAX] {
        for (i, x) in SAMPLES.into_iter().enumerate() {
            let (y, z) = (SAMPLES[(i + 3) % 8], SAMPLES[(i + 5) % 8]);
            let faction = (i % 4) as u8;
            let mut out = BytesMut::new();
            WarCodec.encode(id, &wire(x, y, z, faction), &mut out);
            let typed = UnitRecord {
                entity: id,
                x,
                y,
                z,
                kind: crate::war::Kind::Player as i32,
                faction: u32::from(faction),
                hp: 75,
            };
            assert_eq!(
                &out[..],
                &typed.encode_to_vec()[..],
                "({id}, {x}, {y}, {z})"
            );
        }
    }
    let mut out = BytesMut::new();
    WarCodec.encode(5, &wire(10, 0, 20, 0), &mut out);
    let decoded = UnitRecord::decode(&out[..]).expect("a record");
    assert_eq!((decoded.y, decoded.faction), (0, 0));
    assert_eq!(out.len(), 2 + 2 + 2 + 2 + 2, "entity, x, z, kind, hp");
}

/// Every ground coordinate on the map stays a 2-byte varint.
#[test]
fn a_map_corner_record_is_small() {
    let mut out = BytesMut::new();
    let corner = wire(-to_dm(WORLD_HALF), 120, to_dm(WORLD_HALF), 3);
    WarCodec.encode(1, &corner, &mut out);
    // entity 2, x 3, y 3, z 3, kind 2, faction 2, hp 2.
    assert_eq!(out.len(), 17);
}

/// The wire's `Planar` is in metres (the position's unit): the shard
/// grid places a record and its unit on the same side of every seam.
#[test]
fn the_shard_grid_reads_the_wire_in_metres() {
    let grid = partition();
    for (x, z) in [(-12.3, 45.6), (0.1, -0.1), (399.9, 400.0), (-799.0, 799.0)] {
        let pos = Pos3::ground(x, z);
        let w = WarWire::of(
            &pos,
            &Unit {
                kind: Kind::Player,
                faction: None,
                hp: 1,
            },
        );
        assert_eq!(
            Partition::<WarWire>::region_of(&grid, &pos),
            crate::world::home_shard(&pos),
            "({x}, {z})"
        );
        assert_eq!(w.planar(), [w.x.div_euclid(10), w.z.div_euclid(10)]);
        assert_eq!(
            w.planar(),
            [pos.x.floor() as i32, pos.z.floor() as i32],
            "({x}, {z}): metres"
        );
    }
}
