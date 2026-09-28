//! [`GridPartition3`], the volumetric shard preset (BACKLOG A5), over
//! the 3D game's types (height is the second axis, y): box regions that
//! own their lower faces, the 6- or 26-neighbourhood, the strip across a
//! horizontal face, the band and the unit check — and a one-layer grid is
//! the 2D preset.

use std::collections::HashSet;

use super::*;

fn region(g: &GridPartition3<Pos3>, p: Pos3) -> usize {
    Partition::<Wire3>::region_of(g, &p)
}

fn neighbors(g: &GridPartition3<Pos3>, idx: usize) -> Vec<usize> {
    Partition::<Wire3>::neighbors(g, idx)
}

/// 2×3×2 over `[-60, 60]³`: edges 60 (x), 40 (y), 60 (z); the index is
/// `(z · 3 + y) · 2 + x`. A region owns its lower face on every axis, a
/// point on the cube's upper face or off the map is clamped into the
/// grid, and every region owns part of the map.
#[test]
fn grid_partition3_regions_own_their_lower_faces() {
    let g = GridPartition3::new([2, 3, 2], 60.0);
    assert_eq!(Partition::<Wire3>::shard_count(&g), 12);
    assert_eq!(region(&g, pos(-30.0, -50.0, -30.0)), 0);
    assert_eq!(region(&g, pos(0.0, -50.0, -30.0)), 1, "x = 0 is the upper");
    assert_eq!(region(&g, pos(-0.01, -50.0, -30.0)), 0);
    assert_eq!(region(&g, pos(-30.0, -20.0, -30.0)), 2, "y = -20 is row 1");
    assert_eq!(region(&g, pos(-30.0, -20.01, -30.0)), 0);
    assert_eq!(region(&g, pos(-30.0, -50.0, 0.0)), 6, "z = 0 is layer 1");
    assert_eq!(region(&g, pos(-30.0, -50.0, -0.01)), 0);
    assert_eq!(region(&g, pos(60.0, 60.0, 60.0)), 11, "the upper corner");
    assert_eq!(region(&g, pos(1e6, -1e6, 1e6)), 7, "clamped");

    let mut owned = HashSet::new();
    for i in 0..=24 {
        for j in 0..=24 {
            for k in 0..=24 {
                let at = |s: i32| -66.0 + 132.0 * s as f32 / 24.0;
                let r = region(&g, pos(at(i), at(j), at(k)));
                assert!(r < 12, "one owner in range: {r}");
                owned.insert(r);
            }
        }
    }
    assert_eq!(owned.len(), 12, "every region has volume");
}

/// On a 3×3×3 grid the centre has its six faces by default — in the
/// order −x, +x, −y, +y, −z, +z — and all 26 regions around it with the
/// diagonals, faces first, then the twelve edges, then the eight
/// corners. Fewer at the grid's faces, edges and corners; the relation
/// is symmetric on every shape.
#[test]
fn grid_partition3_has_6_face_neighbours_or_26() {
    let plain = GridPartition3::<Pos3>::new([3, 3, 3], 60.0);
    let full = GridPartition3::<Pos3>::new([3, 3, 3], 60.0).with_diagonals();
    assert_eq!(neighbors(&plain, 13), vec![12, 14, 10, 16, 4, 22]);
    let all = neighbors(&full, 13);
    assert_eq!(all.len(), 26);
    assert_eq!(all[..6], [12, 14, 10, 16, 4, 22], "faces first");
    let set: HashSet<usize> = all.iter().copied().collect();
    assert_eq!(set, (0..27).filter(|&i| i != 13).collect(), "distinct");
    assert_eq!(all[6..10], [1, 3, 5, 7], "the four lower edges first");
    assert_eq!(all[18..], [0, 2, 6, 8, 18, 20, 24, 26], "the corners last");

    // (slot) → (default, diagonals): a grid corner, an edge of the grid,
    // the centre of a grid face.
    for (idx, six, all) in [(0, 3, 7), (1, 4, 11), (4, 5, 17), (26, 3, 7)] {
        assert_eq!(neighbors(&plain, idx).len(), six, "region {idx}");
        assert_eq!(neighbors(&full, idx).len(), all, "region {idx}");
    }

    for shape in [
        [1, 1, 1],
        [2, 1, 1],
        [1, 1, 2],
        [2, 2, 2],
        [4, 2, 3],
        [1, 4, 2],
    ] {
        for g in [
            GridPartition3::<Pos3>::new(shape, 60.0),
            GridPartition3::<Pos3>::new(shape, 60.0).with_diagonals(),
        ] {
            let n = Partition::<Wire3>::shard_count(&g);
            for a in 0..n {
                for b in neighbors(&g, a) {
                    assert!(b < n && b != a, "{shape:?}: {a}→{b}");
                    assert!(neighbors(&g, b).contains(&a), "{shape:?}: {a}↔{b}");
                }
            }
        }
    }
}

/// Two regions stacked on the height axis (`[1, 2, 1]` over `[-64,
/// 64]³`: region 0 is y < 0; border margin 16), across the horizontal
/// face between them. The lower region exports what is strictly within
/// the margin of its ceiling; the upper one admits a record within the
/// margin below its floor, inclusive — the 2D preset's `<` / `≤` — on
/// every axis.
#[test]
fn grid_partition3_lends_across_a_horizontal_face() {
    let g = GridPartition3::<Pos3>::new([1, 2, 1], 64.0);
    let exports = |y: f32| Partition::<Wire3>::exports(&g, 0, &pos(0.0, y, 0.0));
    assert!(exports(-1.0) && exports(-15.9), "near the ceiling");
    assert!(!exports(-16.0) && !exports(-30.0), "deeper");
    assert!(exports(-50.0), "near the cube's lower face (no neighbour)");
    let admits = |x: i32, y: i32| Partition::<Wire3>::admits(&g, 1, &wire(x, y, 0));
    assert!(admits(0, -16) && admits(0, 80), "the margin, inclusive");
    assert!(!admits(0, -17) && !admits(0, 81), "beyond it");
    assert!(admits(80, 0) && !admits(81, 0), "on every axis");
}

/// Crystallization's band: the region, or strictly less than the margin
/// outside it on the worst of the three axes — clamped to the border
/// margin (128 m on `[1, 2, 2]` over `[-512, 512]³`).
#[test]
fn grid_partition3_holds_within_the_clamped_margin() {
    let g = GridPartition3::<Pos3>::new([1, 2, 2], 512.0); // region 0: y, z < 0
    let holds = |[x, y, z]: [f32; 3], m: f32| Partition::<Wire3>::holds(&g, 0, &pos(x, y, z), m);
    assert!(holds([0.0, -300.0, -300.0], 0.0), "inside, any margin");
    assert!(
        holds([0.0, -900.0, -300.0], 0.0),
        "below the map: its region's"
    );
    assert!(holds([0.0, 63.9, -300.0], 64.0), "above y = 0…");
    assert!(!holds([0.0, 64.0, -300.0], 64.0), "…strictly");
    assert!(holds([0.0, -300.0, 63.9], 64.0), "past z = 0…");
    assert!(!holds([0.0, -300.0, 64.0], 64.0), "…strictly");
    assert!(
        !holds([560.0, 50.0, -300.0], 40.0),
        "an edge: the worst axis…"
    );
    assert!(holds([530.0, 30.0, -300.0], 40.0));
    assert!(!holds([0.0, 30.0, 50.0], 40.0), "…the third one too");
    assert!(holds([0.0, 30.0, 35.0], 40.0));
    assert!(holds([0.0, 127.0, -300.0], f32::INFINITY), "a wide margin…");
    assert!(!holds([0.0, 128.0, -300.0], f32::INFINITY), "…stops at 128");
}

/// A one-layer grid is the 2D preset over the same map: shape `[cols,
/// rows, 1]` against `GridPartition2::new(rows · cols)` (whose plane is
/// this game's (x, z)), on points whose two "second axes" agree —
/// regions, both neighbourhoods, exports, frame filter and band.
#[test]
fn a_one_layer_grid_partition3_is_grid_partition2() {
    for n in [1usize, 2, 4, 6, 8, 12, 16] {
        let (rows, cols) = grid_shape(n);
        for diagonals in [false, true] {
            let (mut g2, mut g3) = (
                GridPartition2::<Pos3>::new(n, 50.0),
                GridPartition3::<Pos3>::new([cols, rows, 1], 50.0),
            );
            if diagonals {
                (g2, g3) = (g2.with_diagonals(), g3.with_diagonals());
            }
            for idx in 0..n {
                let n2 = Partition::<Wire3>::neighbors(&g2, idx);
                assert_eq!(neighbors(&g3, idx), n2, "n={n} {diagonals}: {idx}");
            }
            for i in -12..=12 {
                for j in -12..=12 {
                    let (x, v) = (i as f32 * 5.0, j as f32 * 5.0);
                    let (p, w) = (pos(x, v, v), wire(x as i32, v as i32, v as i32));
                    let r2 = Partition::<Wire3>::region_of(&g2, &p);
                    assert_eq!(region(&g3, p), r2, "n={n}: ({x}, {v})");
                    for idx in 0..n {
                        let same = |f: &dyn Fn(&dyn Partition<Wire3, Pos = Pos3>) -> bool| {
                            assert_eq!(f(&g3), f(&g2), "n={n} idx={idx}: ({x}, {v})");
                        };
                        same(&|g| g.exports(idx, &p));
                        same(&|g| g.admits(idx, &w));
                        same(&|g| g.holds(idx, &p, 6.0));
                    }
                }
            }
        }
    }
}

/// The unit contract, as in 2D: a wire in the position's unit passes,
/// one off by more than the border margin on any axis is caught.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "GridPartition3: an entity at [20.0, 0.0, -100.0]")]
fn grid_partition3_checks_the_wire_unit_on_every_axis() {
    let g = GridPartition3::<Pos3>::new([2, 2, 2], 512.0); // margin 128
    let p = pos(20.0, 0.0, -100.0);
    Partition::<Wire3>::debug_check_wire(&g, &p, &wire(20, 127, -100));
    Partition::<Wire3>::debug_check_wire(&g, &p, &wire(20, 0, -1_000));
}

/// A shape with no region, or more than 256, is refused.
#[test]
#[should_panic(expected = "1..=256 regions")]
fn grid_partition3_refuses_an_empty_shape() {
    let _ = GridPartition3::<Pos3>::new([4, 0, 4], 50.0);
}
