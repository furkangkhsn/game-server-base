//! [`SectorMap`] — the PVS seam over static map data (KIT-ARCHITECTURE
//! §4.2), and [`ConvexSectors2`], its 2D preset (§7): hand-authored
//! convex polygons on the ground plane plus a static visibility table.
//!
//! The map data (the polygons, the table) is the game's (§2); the room
//! only ever does the two lookups — the sector of a position, and the
//! sectors visible from a sector.

use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;

use bevy_ecs::component::Component;

use crate::kit::space::Planar;

/// A static partition of the map into sectors plus the precomputed
/// visibility between them (a PVS): the sector room groups connections
/// by sector and a sector's snapshot is the union of the sectors
/// visible from it.
pub trait SectorMap: Send + 'static {
    /// The position component sectors are looked up by.
    type Pos: Component;

    /// The sector key (the sector room's `GroupKey`).
    type Sector: Eq + Hash + Copy + Debug + Send + 'static;

    /// The sector containing `pos`.
    fn sector_of(&self, pos: &Self::Pos) -> Self::Sector;

    /// The containment sector: what an entity outside every sector of
    /// the map lands in — and where the room groups what it cannot place
    /// at all (a player without an entity, an entity without a
    /// position). It must see only itself and be seen by no map sector,
    /// so nothing unplaced leaks into the map's visibility.
    fn outside(&self) -> Self::Sector;

    /// The sectors visible from `sector` (itself included), in a fixed
    /// order.
    fn visible_from(&self, sector: Self::Sector) -> impl Iterator<Item = Self::Sector> + '_;
}

/// A sector of a [`ConvexSectors2`] map: its index in the polygon list
/// (the containment sector is the index one past the last polygon).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sector(pub u8);

/// The 2D PVS preset: convex polygons on the ground plane (any position
/// component with an `f32` [`Planar`] projection) and a static
/// visibility table. A position on a shared edge belongs to the first
/// polygon in list order (deterministic); a position outside every
/// polygon is in the containment sector ([`SectorMap::outside`]), which
/// sees only itself.
pub struct ConvexSectors2<P> {
    /// The sector polygons (index = sector), convex, counter-clockwise.
    polygons: Vec<Vec<(f32, f32)>>,
    /// Per sector (the containment sector last): the bitmask of the
    /// sectors visible from it.
    visible: Vec<u16>,
    _pos: PhantomData<fn() -> P>,
}

impl<P> ConvexSectors2<P> {
    /// A map of `polygons` (convex, counter-clockwise; index = sector)
    /// where `visible_from[s]` lists the sectors visible from sector
    /// `s` (itself included). The containment sector is added by the
    /// preset: it sees only itself.
    ///
    /// # Panics
    ///
    /// When `visible_from` does not have one entry per polygon, or a
    /// listed sector is not a polygon of the map.
    #[must_use]
    pub fn new(polygons: Vec<Vec<(f32, f32)>>, visible_from: Vec<Vec<Sector>>) -> Self {
        assert_eq!(
            visible_from.len(),
            polygons.len(),
            "one visibility entry per sector"
        );
        let out = polygons.len();
        let mut visible: Vec<u16> = visible_from
            .iter()
            .map(|list| {
                list.iter().fold(0, |mask, s| {
                    assert!(usize::from(s.0) < out, "{s:?} is not a sector of the map");
                    mask | (1 << s.0)
                })
            })
            .collect();
        visible.push(1 << out);
        Self {
            polygons,
            visible,
            _pos: PhantomData,
        }
    }
}

impl<P: Component + Planar<Coord = f32>> SectorMap for ConvexSectors2<P> {
    type Pos = P;
    type Sector = Sector;

    #[inline]
    fn sector_of(&self, pos: &P) -> Sector {
        let [x, y] = pos.planar();
        for (i, poly) in self.polygons.iter().enumerate() {
            if in_convex(poly, x, y) {
                return Sector(i as u8);
            }
        }
        self.outside()
    }

    #[inline]
    fn outside(&self) -> Sector {
        Sector(self.polygons.len() as u8)
    }

    fn visible_from(&self, sector: Sector) -> impl Iterator<Item = Sector> + '_ {
        let mask = self.visible[usize::from(sector.0)];
        (0..self.visible.len())
            .filter(move |s| mask & (1 << s) != 0)
            .map(|s| Sector(s as u8))
    }
}

/// True when `(x, y)` is inside (or on the edge of) a counter-clockwise
/// convex polygon. A point is inside a convex polygon iff all edge
/// cross products have the same sign (edge points count as inside, so
/// shared sector boundaries are owned by the first sector in test order —
/// deterministic).
fn in_convex(poly: &[(f32, f32)], x: f32, y: f32) -> bool {
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
