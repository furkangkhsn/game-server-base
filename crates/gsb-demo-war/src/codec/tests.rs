use bytes::BytesMut;
use gsb_kit::space::Partition;

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

/// The war rides the kit's record run; its body (`codec/run.rs`) is
/// pinned byte for byte: a walking player of faction 2 at (-12.3, 45.6)
/// m with 75 hp, a tower on its platform, a neutral point.
#[test]
fn the_run_body_is_pinned() {
    const { assert!(<WarCodec as RecordCodec>::RUN) };
    let body = |w: &WarWire| {
        let mut out = BytesMut::new();
        WarCodec.encode(7, w, &mut out);
        out.to_vec()
    };
    let player = WarWire {
        x: -123,
        y: 0,
        z: 456,
        kind: Kind::Player,
        faction: 2,
        hp: 75,
    };
    // head 2 | 2 << 3 = 18; x zigzag 245; z zigzag 912; hp 75.
    assert_eq!(body(&player), [0x12, 0xF5, 0x01, 0x90, 0x07, 0x4B]);
    let tower = WarWire {
        kind: Kind::Tower,
        y: 120,
        hp: 0,
        faction: 3,
        ..player
    };
    // head 1 | 4 | 24 = 29; y zigzag 240 after z; hp 0.
    assert_eq!(
        body(&tower),
        [0x1D, 0xF5, 0x01, 0x90, 0x07, 0xF0, 0x01, 0x00]
    );
    let point = WarWire {
        kind: Kind::Point,
        faction: 0,
        hp: 0,
        ..player
    };
    assert_eq!(body(&point), [0x06, 0xF5, 0x01, 0x90, 0x07, 0x00]);
}

/// Every record round-trips, back to back in one run (the body is
/// self-delimiting: each read ends exactly where the next body starts).
#[test]
fn run_bodies_round_trip_back_to_back() {
    let mut run = BytesMut::new();
    let mut want = Vec::new();
    for (i, x) in SAMPLES.into_iter().enumerate() {
        let (y, z) = (SAMPLES[(i + 3) % 8], SAMPLES[(i + 5) % 8]);
        for (kind, faction, hp) in [
            (Kind::Player, 1, 100),
            (Kind::Tower, 3, 0),
            (Kind::Point, 0, 7),
        ] {
            let w = WarWire {
                x,
                y,
                z,
                kind,
                faction,
                hp,
            };
            WarCodec.encode(9, &w, &mut run);
            want.push(w);
        }
    }
    let mut rest = &run[..];
    for w in want {
        assert_eq!(read_body(&mut rest), Ok(w));
    }
    assert!(rest.is_empty());
}

/// A truncated body, a kind 0 and a faction past a byte are malformed.
#[test]
fn a_bad_run_body_is_malformed() {
    for bad in [
        &[0x12, 0xF5][..],
        &[0x10, 0x00, 0x00, 0x00][..],
        &[0x82, 0x10, 0, 0, 0][..],
    ] {
        assert!(read_body(&mut &bad[..]).is_err(), "{bad:?}");
    }
}

/// On the ground, anywhere on the map, a unit's body is 6 bytes; off
/// the ground (a platform), at a map corner, 8.
#[test]
fn a_map_corner_record_is_small() {
    let mut out = BytesMut::new();
    let corner = wire(-to_dm(WORLD_HALF), 120, to_dm(WORLD_HALF), 3);
    WarCodec.encode(1, &corner, &mut out);
    assert_eq!(out.len(), 8, "head, x 2, z 2, y 2, hp");
    out.clear();
    WarCodec.encode(
        1,
        &wire(to_dm(WORLD_HALF), 0, -to_dm(WORLD_HALF), 1),
        &mut out,
    );
    assert_eq!(out.len(), 6, "head, x 2, z 2, hp");
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
