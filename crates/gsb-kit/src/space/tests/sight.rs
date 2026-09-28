//! Per-unit sight in the vision presets (BACKLOG A8): a unit's own
//! radius decides by the same inclusive squared-distance rule as the
//! preset's radius, is clamped to `[1, MAX_SIGHT_CELLS · radius]`, and
//! the widened neighbourhood covers every cell such a radius reaches —
//! more than the fixed 3×3 / 27 cells once it exceeds the cell.

use std::collections::HashSet;

use super::*;

/// The 2D preset: a unit's own radius sees past the preset's radius and
/// falls short of it, with the boundary included in both directions.
#[test]
fn vision_grid2_a_units_own_radius_decides_inclusively() {
    let v = VisionGrid2::<Pos3>::new(25.0);
    let origin = pos(0.0, 0.0, 0.0);
    let at = |d: f32| pos(0.0, 500.0, d); // the ground plane is (x, z)
    assert!(!v.sees(&origin, &at(60.0)), "beyond the preset's radius");
    assert!(
        v.sees_within(&origin, 60.0, &at(60.0)),
        "on the own boundary"
    );
    assert!(!v.sees_within(&origin, 60.0, &at(60.5)), "past it");
    assert!(v.sees(&origin, &at(20.0)), "the preset's radius sees 20");
    assert!(!v.sees_within(&origin, 10.0, &at(20.0)), "a ward does not");
    assert!(v.sees_within(&origin, 10.0, &at(10.0)), "a ward's boundary");
    assert!(v.sees_within(&origin, 25.0, &at(25.0)) && v.sees(&origin, &at(25.0)));
}

/// The 3D preset: the same rule over the 3D distance (height counts).
#[test]
fn vision_grid3_a_units_own_radius_decides_inclusively() {
    let v = VisionGrid3::<Pos3>::new(15.0);
    let origin = pos(0.0, 0.0, 0.0);
    assert!(!v.sees(&origin, &pos(0.0, 40.0, 0.0)));
    assert!(v.sees_within(&origin, 40.0, &pos(0.0, 40.0, 0.0)));
    assert!(!v.sees_within(&origin, 40.0, &pos(0.0, 40.5, 0.0)));
    assert!(v.sees(&origin, &pos(0.0, 0.0, 12.0)));
    assert!(!v.sees_within(&origin, 5.0, &pos(0.0, 0.0, 12.0)));
}

/// A degenerate radius (zero, negative, NaN) sees as 1; a huge one as
/// `MAX_SIGHT_CELLS` cells — the clamp the widened neighbourhood relies on.
#[test]
fn a_units_own_radius_is_clamped() {
    let v = VisionGrid2::<Pos3>::new(25.0);
    let origin = pos(0.0, 0.0, 0.0);
    for r in [0.0, -30.0, f32::NAN] {
        assert!(v.sees_within(&origin, r, &pos(1.0, 0.0, 0.0)), "{r}");
        assert!(!v.sees_within(&origin, r, &pos(1.5, 0.0, 0.0)), "{r}");
    }
    let most = 25.0 * f32::from(MAX_SIGHT_CELLS);
    for r in [1_000.0, f32::INFINITY] {
        assert!(v.sees_within(&origin, r, &pos(most, 0.0, 0.0)), "{r}");
        assert!(
            !v.sees_within(&origin, r, &pos(most + 0.5, 0.0, 0.0)),
            "{r}"
        );
    }
}

/// The widened neighbourhood: the preset's own block for a reach up to
/// the radius, `(2k + 1)²` / `(2k + 1)³` cells with `k = ⌈reach / cell⌉`
/// beyond it, never more than `MAX_SIGHT_CELLS` rings.
#[test]
fn the_widened_neighbourhood_grows_by_rings() {
    let v2 = VisionGrid2::<Pos3>::new(25.0);
    let v3 = VisionGrid3::<Pos3>::new(25.0);
    let c2 = Cell(3, -1);
    let c3 = Cell3(3, -1, 7);
    let own2: HashSet<Cell> = v2.neighborhood(c2).collect();
    let own3: HashSet<Cell3> = v3.neighborhood(c3).collect();
    for reach in [0.0, 10.0, 25.0, f32::NAN] {
        assert_eq!(
            v2.neighborhood_within(c2, reach).collect::<HashSet<_>>(),
            own2
        );
        assert_eq!(
            v3.neighborhood_within(c3, reach).collect::<HashSet<_>>(),
            own3
        );
    }
    for (reach, k) in [(25.5, 2), (50.0, 2), (60.0, 3), (100.0, 4), (1e9, 4)] {
        let side = 2 * k + 1;
        let hood2: HashSet<Cell> = v2.neighborhood_within(c2, reach).collect();
        assert_eq!(hood2.len(), side * side, "reach {reach}");
        assert!(hood2.is_superset(&own2));
        let hood3: HashSet<Cell3> = v3.neighborhood_within(c3, reach).collect();
        assert_eq!(hood3.len(), side * side * side, "reach {reach}");
        assert!(hood3.is_superset(&own3));
    }
}

/// The widened grid contract over a lattice of pairs straddling cell
/// boundaries: whenever a viewer with radius `r` sees a target, its cell
/// is in `neighborhood_within(cell(target), r)` — for radii below, at
/// and well beyond the cell (the fixed 3×3 misses the latter).
#[test]
fn the_widened_neighbourhood_covers_every_own_radius() {
    let v = VisionGrid2::<Pos3>::new(10.0);
    let mut beyond_3x3 = 0;
    for r in [4.0f32, 10.0, 17.5, 20.0, 33.3, 40.0] {
        let steps: Vec<f32> = [-1.0f32, -0.75, -0.5, -0.01, 0.0, 0.01, 0.5, 0.75, 1.0]
            .iter()
            .map(|f| f * r)
            .collect();
        for &tx in &[-0.5f32, 9.9, 20.0, -35.0] {
            let target = pos(tx, 0.0, -tx);
            for &dx in &steps {
                for &dz in &steps {
                    let viewer = pos(tx + dx, 0.0, -tx + dz);
                    if !v.sees_within(&viewer, r, &target) {
                        continue;
                    }
                    let cell = v.cell(&viewer);
                    assert!(
                        v.neighborhood_within(v.cell(&target), r).any(|c| c == cell),
                        "r {r}: {viewer:?} sees {target:?} from outside the block"
                    );
                    if !v.neighborhood(v.cell(&target)).any(|c| c == cell) {
                        beyond_3x3 += 1;
                    }
                }
            }
        }
    }
    assert!(beyond_3x3 > 20, "the lattice left the 3×3 ({beyond_3x3})");
}

/// A model without per-unit sight keeps its own rule: the trait's
/// defaults ignore the radius and widen nothing.
#[test]
fn a_model_without_per_unit_sight_ignores_the_radius() {
    /// Sees along its own row only (a toy rule of its own).
    struct Rows;
    impl Vision for Rows {
        type Pos = Pos3;
        type Cell = i32;
        fn cell(&self, p: &Pos3) -> i32 {
            p.z as i32
        }
        fn neighborhood(&self, cell: i32) -> impl Iterator<Item = i32> {
            std::iter::once(cell)
        }
        fn sees(&self, v: &Pos3, t: &Pos3) -> bool {
            v.z as i32 == t.z as i32
        }
    }
    let (a, b) = (pos(0.0, 0.0, 3.0), pos(900.0, 0.0, 3.0));
    assert!(Rows.sees_within(&a, 1.0, &b));
    assert!(!Rows.sees_within(&a, 1e6, &pos(0.0, 0.0, 4.0)));
    assert_eq!(Rows.neighborhood_within(3, 1e6).collect::<Vec<_>>(), [3]);
}
