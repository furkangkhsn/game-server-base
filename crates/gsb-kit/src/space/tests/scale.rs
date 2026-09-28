//! The partition presets' wire scale (BACKLOG A7): a game whose wire is
//! quantized finer than its position (centimetres on the wire, metres
//! in the simulation) declares the ratio with `with_wire_scale(100.0)`
//! instead of coarsening its wire's projection. The presets divide
//! every wire value they compare with a region by it (the frame filter
//! and the unit check); the position side (regions, export, band) never
//! reads it.

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

/// Wire units per metre of the centimetre wire.
const CM: f32 = 100.0;

/// The 3D game's wire in its OWN unit (centimetres, or any other scale),
/// projected in that unit.
#[derive(Debug, Clone, Copy)]
struct Scaled {
    x: i32,
    y: i32,
    z: i32,
}

impl Planar for Scaled {
    type Coord = i32;
    fn planar(&self) -> [i32; 2] {
        [self.x, self.z]
    }
}

impl Spatial for Scaled {
    type Coord = i32;
    fn spatial(&self) -> [i32; 3] {
        [self.x, self.y, self.z]
    }
}

/// The wire of `p` at `scale` units per metre, rounded (or truncated).
fn at(p: Pos3, scale: f32, round: bool) -> Scaled {
    let q = |v: f32| {
        let v = v * scale;
        (if round { v.round() } else { v }) as i32
    };
    Scaled {
        x: q(p.x),
        y: q(p.y),
        z: q(p.z),
    }
}

fn cm(p: Pos3) -> Scaled {
    at(p, CM, true)
}

fn scaled(x: i32, y: i32, z: i32) -> Scaled {
    Scaled { x, y, z }
}

/// 2×2 over `[-512, 512]²` metres: region 0 is x, z < 0; margin 128 m.
fn plane() -> GridPartition2<Pos3> {
    GridPartition2::new(4, 512.0)
}

/// 2×2×2 over `[-64, 64]³` metres: region 0 is x, y, z < 0, region 2
/// sits on it (y ≥ 0); margin 16 m.
fn cube() -> GridPartition3<Pos3> {
    GridPartition3::new([2, 2, 2], 64.0)
}

/// In whole metres the centimetre record is admitted exactly where the
/// metre record is (every seam from both sides, the margin inclusive,
/// off the map too) and scale 1 is the default; below a metre the frame
/// ends at 128 m to the centimetre. Unscaled, 20 m reads as 2 km. The
/// position side is the scale's no business.
#[test]
fn grid_partition2_reads_a_centimetre_wire_at_its_scale() {
    let (plain, g, one) = (
        plane(),
        plane().with_wire_scale(CM),
        plane().with_wire_scale(1.0),
    );
    for i in -75..=75 {
        for j in -75..=75 {
            let (x, z) = (i * 8, j * 8);
            let (m, c) = (wire(x, 0, z), scaled(x * 100, 0, z * 100));
            for idx in 0..4 {
                let want = Partition::<Wire3>::admits(&plain, idx, &m);
                let got = Partition::<Scaled>::admits(&g, idx, &c);
                assert_eq!(got, want, "region {idx}: ({x}, {z}) m");
                assert_eq!(Partition::<Wire3>::admits(&one, idx, &m), want);
            }
        }
    }
    let admits = |g: &GridPartition2<Pos3>, idx, x| {
        Partition::<Scaled>::admits(g, idx, &scaled(x, 0, -5_000))
    };
    assert!(admits(&g, 0, 2_000) && admits(&g, 0, 12_800), "20 m, 128 m");
    assert!(!admits(&g, 0, 12_801), "128.01 m is past the margin");
    assert!(
        admits(&g, 1, -12_800) && !admits(&g, 1, -12_801),
        "the east side"
    );
    assert!(!admits(&plain, 0, 2_000), "unscaled, 20 m reads as 2 km");

    for i in -13..=13 {
        for j in -13..=13 {
            let p = pos(i as f32 * 41.3, 7.0, j as f32 * 41.3);
            let r = Partition::<Scaled>::region_of(&plain, &p);
            assert_eq!(Partition::<Scaled>::region_of(&g, &p), r);
            for idx in 0..4 {
                let (e, h) = (Partition::<Scaled>::exports, Partition::<Scaled>::holds);
                assert_eq!(e(&g, idx, &p), e(&plain, idx, &p));
                assert_eq!(h(&g, idx, &p, 40.0), h(&plain, idx, &p, 40.0));
            }
        }
    }
}

/// The volumetric preset, the same way on every axis: whole metres as
/// the metre wire, the height face to the centimetre, unscaled 10 m
/// reads as 1 km; regions, export and band ignore the scale.
#[test]
fn grid_partition3_reads_a_centimetre_wire_at_its_scale() {
    let (plain, g) = (cube(), cube().with_wire_scale(CM));
    for i in -9..=9 {
        for j in -9..=9 {
            for k in -9..=9 {
                let [x, y, z] = [i * 8, j * 8, k * 8];
                let (m, c) = (wire(x, y, z), scaled(x * 100, y * 100, z * 100));
                for idx in 0..8 {
                    let want = Partition::<Wire3>::admits(&plain, idx, &m);
                    let got = Partition::<Scaled>::admits(&g, idx, &c);
                    assert_eq!(got, want, "region {idx}: ({x}, {y}, {z}) m");
                }
                let p = pos(x as f32 + 0.3, y as f32 - 0.6, z as f32 + 0.9);
                let r = Partition::<Scaled>::region_of(&plain, &p);
                assert_eq!(Partition::<Scaled>::region_of(&g, &p), r);
                for idx in 0..8 {
                    let (e, h) = (Partition::<Scaled>::exports, Partition::<Scaled>::holds);
                    assert_eq!(e(&g, idx, &p), e(&plain, idx, &p));
                    assert_eq!(h(&g, idx, &p, 6.0), h(&plain, idx, &p, 6.0));
                }
            }
        }
    }
    let admits = |g: &GridPartition3<Pos3>, idx, y| {
        Partition::<Scaled>::admits(g, idx, &scaled(-1_000, y, -1_000))
    };
    assert!(
        admits(&g, 0, 1_000) && admits(&g, 0, 1_600),
        "10 m, 16 m up"
    );
    assert!(!admits(&g, 0, 1_601), "16.01 m is past the margin");
    assert!(
        admits(&g, 2, -1_600) && !admits(&g, 2, -1_601),
        "from above"
    );
    assert!(!admits(&plain, 0, 1_000), "unscaled, 10 m reads as 1 km");
}

/// A one-layer volumetric grid is still the 2D preset at the same scale
/// — a finer wire (centimetres) and a coarser one (2 m units): the
/// frame filter agrees on every region for sub-metre points, and both
/// unit checks pass.
#[test]
fn a_one_layer_grid_partition3_is_grid_partition2_at_the_same_scale() {
    for scale in [CM, 0.5] {
        for n in [1usize, 2, 4, 6, 8, 12, 16] {
            let (rows, cols) = grid_shape(n);
            let g2 = GridPartition2::<Pos3>::new(n, 50.0).with_wire_scale(scale);
            let g3 = GridPartition3::<Pos3>::new([cols, rows, 1], 50.0).with_wire_scale(scale);
            for i in -12..=12 {
                for j in -12..=12 {
                    let (x, v) = (i as f32 * 5.0 + 0.37, j as f32 * 5.0 - 0.21);
                    let p = pos(x, v, v);
                    let w = at(p, scale, true);
                    Partition::<Scaled>::debug_check_wire(&g2, &p, &w);
                    Partition::<Scaled>::debug_check_wire(&g3, &p, &w);
                    for idx in 0..n {
                        let a2 = Partition::<Scaled>::admits(&g2, idx, &w);
                        let a3 = Partition::<Scaled>::admits(&g3, idx, &w);
                        assert_eq!(a3, a2, "scale {scale} n={n} idx={idx}: ({x}, {v})");
                    }
                }
            }
        }
    }
}

/// The unit check takes a centimetre wire, rounded or truncated, inside
/// the map or strayed outside it — at the scale; the same records fail
/// it without the scale (debug builds), so the scale is what makes the
/// two values consistent.
#[test]
fn the_scale_is_what_makes_the_unit_check_pass() {
    let (g2, g3) = (plane().with_wire_scale(CM), cube().with_wire_scale(CM));
    for x in [-700.0f32, -511.6, -0.004, 0.0, 0.6, 127.99, 511.5, 900.2] {
        for z in [-600.3f32, -1.5, 0.0, 300.7] {
            let p = pos(x, z / 2.0, z);
            for w in [cm(p), at(p, CM, false)] {
                Partition::<Scaled>::debug_check_wire(&g2, &p, &w);
                Partition::<Scaled>::debug_check_wire(&g3, &p, &w);
            }
        }
    }
    let p = pos(20.0, 0.0, -10.0);
    let passes = |check: &dyn Fn()| catch_unwind(AssertUnwindSafe(check)).is_ok();
    assert!(passes(&|| Partition::<Scaled>::debug_check_wire(
        &g2,
        &p,
        &cm(p)
    )));
    assert!(passes(&|| Partition::<Scaled>::debug_check_wire(
        &g3,
        &p,
        &cm(p)
    )));
    if cfg!(debug_assertions) {
        let (plain2, plain3) = (plane(), cube());
        let check2 = || Partition::<Scaled>::debug_check_wire(&plain2, &p, &cm(p));
        let check3 = || Partition::<Scaled>::debug_check_wire(&plain3, &p, &cm(p));
        assert!(!passes(&check2), "2D: caught without the scale");
        assert!(!passes(&check3), "3D: caught without the scale");
    }
}

/// A scale that is not a positive finite number is refused by both
/// presets; a finer, a coarser and the neutral one are taken.
#[test]
fn a_wire_scale_must_be_positive_and_finite() {
    let takes = |s: f32| {
        let two = catch_unwind(move || plane().with_wire_scale(s)).is_ok();
        let three = catch_unwind(move || cube().with_wire_scale(s)).is_ok();
        assert_eq!(two, three, "both presets agree on {s}");
        two
    };
    for s in [
        0.0,
        -0.0,
        -100.0,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ] {
        assert!(!takes(s), "{s} is refused");
    }
    for s in [0.5, 1.0, 10.0, CM] {
        assert!(takes(s), "{s} is taken");
    }
}
