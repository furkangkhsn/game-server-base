//! The demo's PVS map (hand-written, convex, tiles the [-50, 50]² arena):
//! the sectors, the precomputed static visibility table, and the
//! point-in-sector lookup the sector room groups by (the future
//! `SectorMap`, KIT-ARCHITECTURE §4.2 — map data is the game's, §2).
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
//!   which sees only itself: the broadcast set stays "has a `Position`"
//!   and a runaway entity can never leak into the map's visibility.

use crate::demo::components::Position;

/// A map sector — the PVS group key (a region *inside* a room; deliberately
/// not `RoomId`, which names a gsb room).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sector(pub u8);

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
/// Positions outside every hand-written sector (a target beyond the map).
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

/// The precomputed **static** visibility table: for each sector, a bitmask
/// of the sectors visible *from* it (itself included). This is the PVS —
/// the hand-authored map geometry (transitions/sightlines) frozen into a
/// table the runtime only reads.
///
/// - `A` sees `A` and `C` (open passage north of the west lobby).
/// - `B` sees `B` and `D` (open passage north of the east lobby).
/// - `C` sees `A`, `C` and `D` (plus the open north sightline to `D`).
/// - `D` sees `B`, `C` and `D`.
/// - `A` and `B` see NEITHER each other: they are geometrically adjacent
///   (3 units apart at the closest) but separated by a wall — the table
///   is the law, not the distance.
/// - `OUT` sees only itself (a runaway entity leaks into nothing).
pub(crate) const VISIBLE_FROM: [u16; 5] = [
    1 << SECTOR_WEST | 1 << SECTOR_NW,
    1 << SECTOR_EAST | 1 << SECTOR_NE,
    1 << SECTOR_WEST | 1 << SECTOR_NW | 1 << SECTOR_NE,
    1 << SECTOR_EAST | 1 << SECTOR_NW | 1 << SECTOR_NE,
    1 << SECTOR_OUT,
];

/// True when `(x, y)` is inside (or on the edge of) a counter-clockwise
/// convex polygon. A point is inside a convex polygon iff all edge
/// cross products have the same sign (edge points count as inside, so
/// shared sector boundaries are owned by the first sector in test order —
/// deterministic).
fn in_convex(poly: &[(f32, f32); 4], x: f32, y: f32) -> bool {
    let mut sign = 0.0f32;
    let n = poly.len();
    for i in 0..n {
        let (ax, ay) = poly[i];
        let (bx, by) = poly[(i + 1) % n];
        let cross = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
        if cross.abs() < 1e-9 {
            continue; // on the edge
        }
        let s = cross.signum();
        if sign == 0.0 {
            sign = s;
        } else if s != sign {
            return false;
        }
    }
    true
}

/// The sector containing `pos`: the first (fixed order) sector whose
/// polygon contains it, or [`SECTOR_OUT`] when outside all of them.
#[inline]
pub(crate) fn sector_of(pos: Position) -> Sector {
    for (i, poly) in SECTORS.iter().enumerate() {
        if in_convex(poly, pos.x, pos.y) {
            return Sector(i as u8);
        }
    }
    Sector(SECTOR_OUT)
}
