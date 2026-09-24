//! The 2D presets read a game's types only through [`Planar`]: a 3D game
//! whose world lives on the ground plane (x, z) uses them unchanged by
//! implementing the accessor for its own position and wire types — the
//! phase-4 MMO's path (KIT-ARCHITECTURE §7, §12). The 3D vision preset
//! reads the same position through [`Spatial`] — the phase-3 arena's
//! path, where height separates.

use bevy_ecs::component::Component;

use super::*;

mod units;

/// A 3D game's simulation position: the kit has never seen this type.
#[derive(Debug, Clone, Copy, Component)]
struct Pos3 {
    x: f32,
    y: f32,
    z: f32,
}

/// The game's projection: the ground plane is (x, z); y is height.
impl Planar for Pos3 {
    type Coord = f32;
    fn planar(&self) -> [f32; 2] {
        [self.x, self.z]
    }
}

/// The same position in space, for the 3D presets (axis order x, y, z).
impl Spatial for Pos3 {
    type Coord = f32;
    fn spatial(&self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }
}

/// The same game's quantized 3D wire value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wire3 {
    x: i32,
    y: i32,
    z: i32,
}

impl Planar for Wire3 {
    type Coord = i32;
    fn planar(&self) -> [i32; 2] {
        [self.x, self.z]
    }
}

fn pos(x: f32, y: f32, z: f32) -> Pos3 {
    Pos3 { x, y, z }
}

fn wire(x: i32, y: i32, z: i32) -> Wire3 {
    Wire3 { x, y, z }
}

/// `Grid2` AOI cells, `GridPartition2` regions and border frames,
/// `VisionGrid2` and `ConvexSectors2` over the (x, z) plane of 3D types:
/// the second planar axis is z, and height never moves a cell, a region,
/// a vision test or a sector.
#[test]
fn a_ground_plane_3d_game_uses_the_planar_presets_unchanged() {
    let p = pos(1.0, 2.0, 3.0);
    assert_eq!(
        (p.planar(), p.y),
        ([1.0, 3.0], 2.0),
        "the plane drops height"
    );

    let grid = Grid2::new(20.0);
    assert_eq!(grid.cell_of(&wire(5, 0, 45)), Cell(0, 2));
    assert_eq!(grid.cell_of(&wire(5, 9_000, 45)), Cell(0, 2), "height");

    // 2×2 over [-50, 50]² in (x, z): region 3 is x ≥ 0, z ≥ 0.
    let part = GridPartition2::<Pos3>::new(4, 50.0);
    let region = |p: &Pos3| Partition::<Wire3>::region_of(&part, p);
    assert_eq!(region(&pos(10.0, -300.0, 10.0)), 3);
    assert_eq!(region(&pos(10.0, 300.0, -10.0)), 1, "z picks the row");
    assert!(Partition::<Wire3>::exports(&part, 3, &pos(1.0, 50.0, 30.0)));
    assert!(Partition::<Wire3>::admits(&part, 3, &wire(-1, 0, 20)));
    assert!(!Partition::<Wire3>::admits(&part, 3, &wire(-40, 0, 20)));

    let vision = VisionGrid2::<Pos3>::new(25.0);
    assert!(vision.sees(&pos(0.0, 1_000.0, 0.0), &pos(0.0, -1_000.0, 20.0)));
    assert!(!vision.sees(&pos(0.0, 0.0, 0.0), &pos(0.0, 0.0, 30.0)));

    let sectors = ConvexSectors2::<Pos3>::new(
        vec![vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]],
        vec![vec![Sector(0)]],
    );
    assert_eq!(sectors.sector_of(&pos(5.0, 500.0, 5.0)), Sector(0));
    assert_eq!(sectors.sector_of(&pos(5.0, 5.0, 50.0)), sectors.outside());
}

/// `VisionGrid3` measures in space: height separates two units the
/// planar preset puts on the same spot, the test is the exact 3D
/// distance against the radius, and the radius is clamped like the 2D
/// preset's.
#[test]
fn vision_grid3_sees_by_3d_distance() {
    let v3 = VisionGrid3::<Pos3>::new(25.0);
    let v2 = VisionGrid2::<Pos3>::new(25.0);
    let origin = pos(0.0, 0.0, 0.0);
    let above = pos(0.0, 30.0, 0.0);
    assert!(
        v2.sees(&origin, &above),
        "the ground-plane preset ignores height"
    );
    assert!(
        !v3.sees(&origin, &above),
        "30 straight up is out of a 25 radius"
    );
    assert!(v3.sees(&origin, &pos(0.0, 20.0, 0.0)), "20 up is in sight");
    assert!(
        v3.sees(&origin, &pos(14.0, 14.0, 14.0)),
        "|(14,14,14)| ≈ 24.2"
    );
    assert!(
        !v3.sees(&origin, &pos(15.0, 15.0, 15.0)),
        "|(15,15,15)| ≈ 26.0"
    );
    assert!(
        v3.sees(&origin, &pos(0.0, 0.0, -25.0)),
        "the radius is inclusive"
    );
    assert_eq!(VisionGrid3::<Pos3>::new(0.0).radius(), 1.0, "clamped");
}

/// The neighbourhood is the 27 distinct cells around (and including)
/// the cell, and it honours the grid contract: whenever `sees(v, t)`,
/// the viewer's cell is in `neighborhood(cell(t))` — checked over a
/// lattice of pairs straddling cell boundaries on every axis.
#[test]
fn vision_grid3_neighbourhood_is_27_cells_and_covers_the_radius() {
    let v3 = VisionGrid3::<Pos3>::new(10.0);
    let hood: std::collections::HashSet<Cell3> = v3.neighborhood(Cell3(4, -2, 0)).collect();
    assert_eq!(hood.len(), 27);
    assert!(hood.contains(&Cell3(3, -3, -1)) && hood.contains(&Cell3(5, -1, 1)));
    assert!(
        !hood.contains(&Cell3(6, -2, 0)),
        "two cells away is outside"
    );

    let steps = [-10.0f32, -7.5, -0.1, 0.0, 0.1, 7.5, 10.0];
    let mut checked = 0;
    for &tx in &[-0.5f32, 9.9, 20.0] {
        let target = pos(tx, -tx, tx * 0.5);
        for &dx in &steps {
            for &dy in &steps {
                for &dz in &steps {
                    let viewer = pos(tx + dx, -tx + dy, tx * 0.5 + dz);
                    if v3.sees(&viewer, &target) {
                        checked += 1;
                        assert!(
                            v3.neighborhood(v3.cell(&target))
                                .any(|c| c == v3.cell(&viewer)),
                            "{viewer:?} sees {target:?} from outside the 27 cells"
                        );
                    }
                }
            }
        }
    }
    assert!(
        checked > 100,
        "the lattice exercised the contract ({checked})"
    );
}
