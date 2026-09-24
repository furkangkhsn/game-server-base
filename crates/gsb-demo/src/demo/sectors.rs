//! The demo's PVS map (hand-written, convex, tiles the [-50, 50]² arena):
//! the sectors and the precomputed static visibility table, handed to the
//! kit's 2D sector preset (`ConvexSectors2`, the `SectorMap` of
//! KIT-ARCHITECTURE §4.2 — map data is the game's, §2).
//!
//! ```text
//!        C | D            y
//!  -------+------  x=-10
//!   50    |      (north: C sees A and D; D sees B and C —
//!         |       an open north sightline)
//! -------A------B-----  y=20   (A↔C and B↔D are open passages)
//!  -50 -10|  x=0
//!         |            (A and B are GEOMETRICALLY adjacent — 3 units
//!   -50   |            apart at the closest — but the table says they
//!                 do NOT see each other: a wall. Distance-based AOI
//!                 cannot express this.)
//! ```
//!
//! - `A` (west), `B` (east): the two lobbies, split by a wall at `x = 0`
//!   (`y ∈ [-50, 20]`).
//! - `C` (northwest), `D` (northeast): the north band (`y ∈ [20, 50]`),
//!   split at `x = -10`.
//! - The sectors tile the arena plane, so every arena position is in
//!   exactly one sector (shared edges go to the first sector in the fixed
//!   test order — deterministic). A position outside every sector (a
//!   client sending a target beyond the map) lands in [`SECTOR_OUT`],
//!   (the preset's containment sector), which sees only itself: the
//!   broadcast set stays "has a `Position`" and a runaway entity can
//!   never leak into the map's visibility.

use crate::demo::components::Position;
use gsb_kit::space::{ConvexSectors2, Sector};

/// The hand-written sectors of the demo map (module docs, "The map"):
/// convex polygons, counter-clockwise, tiling the [-50, 50]² arena.
pub(crate) const SECTOR_WEST: u8 = 0;
/// `A`: x ∈ [-50, 0], y ∈ [-50, 20].
pub(crate) const SECTOR_EAST: u8 = 1;
/// `B`: x ∈ [0, 50], y ∈ [-50, 20].
pub(crate) const SECTOR_NW: u8 = 2;
/// `C`: x ∈ [-50, -10], y ∈ [20, 50].
const SECTOR_NE: u8 = 3;
/// `D`: x ∈ [-10, 50], y ∈ [20, 50].
/// Positions outside every hand-written sector (a target beyond the map):
/// the preset's containment sector, one past the last polygon.
pub const SECTOR_OUT: u8 = 4;

/// The sector polygons (index = sector id, counter-clockwise winding).
const SECTORS: [[(f32, f32); 4]; 4] = [
    // A (west)
    [(-50.0, -50.0), (0.0, -50.0), (0.0, 20.0), (-50.0, 20.0)],
    // B (east)
    [(0.0, -50.0), (50.0, -50.0), (50.0, 20.0), (0.0, 20.0)],
    // C (northwest)
    [(-50.0, 20.0), (-10.0, 20.0), (-10.0, 50.0), (-50.0, 50.0)],
    // D (northeast)
    [(-10.0, 20.0), (50.0, 20.0), (50.0, 50.0), (-10.0, 50.0)],
];

/// The precomputed **static** visibility table: for each sector, the
/// sectors visible *from* it (itself included). This is the PVS — the
/// hand-authored map geometry (transitions/sightlines) frozen into a
/// table the runtime only reads.
///
/// - `A` sees `A` and `C` (open passage north of the west lobby).
/// - `B` sees `B` and `D` (open passage north of the east lobby).
/// - `C` sees `A`, `C` and `D` (plus the open north sightline to `D`).
/// - `D` sees `B`, `C` and `D`.
/// - `A` and `B` see NEITHER each other: they are geometrically adjacent
///   (3 units apart at the closest) but separated by a wall — the table
///   is the law, not the distance.
/// - `OUT` sees only itself (the preset's rule: a runaway entity leaks
///   into nothing).
const VISIBLE_FROM: [&[u8]; 4] = [
    &[SECTOR_WEST, SECTOR_NW],
    &[SECTOR_EAST, SECTOR_NE],
    &[SECTOR_WEST, SECTOR_NW, SECTOR_NE],
    &[SECTOR_EAST, SECTOR_NW, SECTOR_NE],
];

/// The demo map as the kit's convex-sector preset over [`Position`].
pub fn demo_map() -> ConvexSectors2<Position> {
    ConvexSectors2::new(
        SECTORS.iter().map(|poly| poly.to_vec()).collect(),
        VISIBLE_FROM
            .iter()
            .map(|list| list.iter().map(|&s| Sector(s)).collect())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gsb_kit::space::SectorMap;

    /// The demo's named out-of-map sector is the preset's containment
    /// sector.
    #[test]
    fn sector_out_is_the_containment_sector() {
        assert_eq!(demo_map().outside(), Sector(SECTOR_OUT));
    }
}
