use bytes::BytesMut;
use gsb_kit::space::{Cell, CellSpace, Partition};
use prost::Message;

use super::*;
use crate::mmo::CellExit;
use crate::world::{CELL_DM, aoi_grid, client_cell, partition};

const SAMPLES: [i32; 10] = [0, 1, -1, 9, -9, 639, -641, 5_120, -5_120, i32::MIN];

fn wire(x: i32, y: i32, z: i32) -> MmoWire {
    MmoWire {
        x,
        y,
        z,
        kind: Kind::Flyer,
        hp: 300,
    }
}

/// Rounded to the nearest decimetre, symmetric around zero, saturating
/// far outside the world.
#[test]
fn quantization_rounds_to_the_nearest_decimetre() {
    let v = Vitals {
        kind: Kind::Mob,
        hp: 7,
    };
    let q = MmoWire::of(&Pos3::new(1.26, 150.0, -0.04), &v);
    assert_eq!((q.x, q.y, q.z), (13, 1500, 0));
    assert_eq!((to_dm(-0.06), to_dm(0.05), to_dm(-512.0)), (-1, 1, -5120));
    assert_eq!(to_dm(1.0e12), i32::MAX);
    assert_eq!(from_dm(-25), -2.5);
}

/// The codec's record body is exactly the typed `EntityRecord`.
#[test]
fn record_body_is_the_entity_record_encoding() {
    for id in [1u64, 127, 128, 3 << 20, u64::MAX] {
        for (i, x) in SAMPLES.into_iter().enumerate() {
            let (y, z) = (SAMPLES[(i + 3) % 10], SAMPLES[(i + 7) % 10]);
            let mut out = BytesMut::new();
            MmoCodec.encode(id, &wire(x, y, z), &mut out);
            let typed = EntityRecord {
                entity: id,
                x,
                y,
                z,
                kind: crate::mmo::Kind::Flyer as i32,
                hp: 300,
            };
            assert_eq!(
                &out[..],
                &typed.encode_to_vec()[..],
                "({id}, {x}, {y}, {z})"
            );
        }
    }
}

/// The wire's ground plane is `[x, z]` in whole metres, and the kit's
/// `Grid2` over it computes exactly the client's cell
/// (`floor(dm / 640)`): height never enters the cell.
#[test]
fn grid2_over_the_wire_is_the_clients_ground_cell() {
    let grid = aoi_grid();
    for x in SAMPLES.into_iter().filter(|v| v.unsigned_abs() < 1 << 24) {
        for z in [0, -1, 639, 640, -640, -641, 3_000] {
            for y in [0, 1_500, 2_000] {
                let Cell(cx, cz) = grid.cell_of(&wire(x, y, z));
                assert_eq!((cx, cz), client_cell(x, z), "({x}, {y}, {z})");
            }
        }
    }
    assert_eq!(client_cell(CELL_DM - 1, -1), (0, -1));
}

/// `Grid2` writes the cell-exit body as the MMO's typed `CellExit` (the
/// second plane index is the z cell).
#[test]
fn grid2_cell_exit_body_is_the_typed_cell_exit() {
    let grid = aoi_grid();
    for (x, z) in [(0, 0), (-1, 3), (7, -8), (-2, 0)] {
        let mut out = BytesMut::new();
        CellSpace::<MmoWire>::encode_cell(&grid, Cell(x, z), &mut out);
        assert_eq!(&out[..], &CellExit { x, z }.encode_to_vec()[..]);
    }
}

/// The shard grid reads the wire in the position's unit (metres): shard
/// 1 (x ≥ 0) admits a neighbour's border record up to its 128 m margin
/// west of the seam and no further (F3 — with a decimetre projection
/// the margin would silently be 12.8 m).
#[test]
fn the_shard_grid_reads_the_wire_in_metres() {
    let grid = partition();
    let at = |x_m: i32| wire(x_m * 10, 0, -1_000);
    assert!(Partition::<MmoWire>::admits(&grid, 1, &at(-127)));
    assert!(!Partition::<MmoWire>::admits(&grid, 1, &at(-129)));
    let pos = Pos3::new(-127.0, 150.0, -100.0);
    assert_eq!(
        Partition::<MmoWire>::region_of(&grid, &pos),
        0,
        "height ignored"
    );
    assert!(Partition::<MmoWire>::exports(&grid, 0, &pos));
}
