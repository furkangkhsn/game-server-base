//! [`Vision`] — the team-fog seam over SIMULATION positions
//! (KIT-ARCHITECTURE §4.2), and its presets (§7): [`VisionGrid2`] on the
//! ground plane, [`VisionGrid3`] in 3D.
//!
//! Team vision is a distance test between units, so it reads the
//! simulation position (not the wire value the AOI space reads). The
//! seam is two things: the exact test ([`Vision::sees`]) and a
//! neighbourhood grid that bounds which units the test has to be run
//! against ([`Vision::cell`] / [`Vision::neighborhood`]) — the room
//! only ever compares units whose cells are neighbours.

use std::hash::Hash;
use std::marker::PhantomData;

use bevy_ecs::component::Component;

use crate::space::{BLOCK_OFFSETS, Cell, Cell3, Planar, Spatial};

/// How a game's units see each other (the team room's vision source
/// model: a unit sees what [`Self::sees`] says it sees).
///
/// **The grid contract:** whenever `sees(viewer, target)` holds, the
/// viewer's cell is in `neighborhood(cell(target))` — the neighbourhood
/// is a SUPERSET of the vision range, and [`Self::sees`] is the only
/// correctness mechanism.
pub trait Vision: Send + 'static {
    /// The position component vision is measured between.
    type Pos: Component + Copy;

    /// The neighbourhood grid's cell key (a per-tick cache key, not a
    /// group key).
    type Cell: Eq + Hash + Copy + Send + 'static;

    /// The grid cell containing `pos`.
    fn cell(&self, pos: &Self::Pos) -> Self::Cell;

    /// The cells whose units may see into `cell` (the grid contract
    /// above), in a fixed order.
    fn neighborhood(&self, cell: Self::Cell) -> impl Iterator<Item = Self::Cell>;

    /// Whether a unit at `viewer` sees a unit at `target`.
    fn sees(&self, viewer: &Self::Pos, target: &Self::Pos) -> bool;
}

/// The 2D vision preset: a uniform vision `radius` on the ground plane
/// (any position component with an `f32` [`Planar`] projection), a grid
/// of `radius`-sized cells, and the 3×3 neighbourhood. A radius-sized
/// cell makes the 3×3 block a superset of the radius disk (a corner
/// cell can hold units up to `radius · √2` away), so the exact
/// squared-distance test decides.
pub struct VisionGrid2<P> {
    radius: f32,
    /// `radius²`, computed once (the per-pair test compares against it).
    radius2: f32,
    _pos: PhantomData<fn() -> P>,
}

impl<P> VisionGrid2<P> {
    /// A vision grid of `radius` world units, clamped to a sane minimum
    /// so a degenerate `0` cannot make vision "only the exact same
    /// point".
    #[must_use]
    pub fn new(radius: f32) -> Self {
        let radius = radius.max(1.0);
        Self {
            radius,
            radius2: radius * radius,
            _pos: PhantomData,
        }
    }

    /// The (clamped) vision radius.
    pub fn radius(&self) -> f32 {
        self.radius
    }
}

impl<P: Component + Copy + Planar<Coord = f32>> Vision for VisionGrid2<P> {
    type Pos = P;
    type Cell = Cell;

    #[inline]
    fn cell(&self, pos: &P) -> Cell {
        let [x, y] = pos.planar();
        Cell(
            (x / self.radius).floor() as i32,
            (y / self.radius).floor() as i32,
        )
    }

    #[inline]
    fn neighborhood(&self, cell: Cell) -> impl Iterator<Item = Cell> {
        BLOCK_OFFSETS
            .into_iter()
            .map(move |(dx, dy)| Cell(cell.0 + dx, cell.1 + dy))
    }

    #[inline]
    fn sees(&self, viewer: &P, target: &P) -> bool {
        let [vx, vy] = viewer.planar();
        let [tx, ty] = target.planar();
        let dx = vx - tx;
        let dy = vy - ty;
        dx * dx + dy * dy <= self.radius2
    }
}

/// The 3D vision preset: a uniform vision `radius` in space (any
/// position component with an `f32` [`Spatial`] projection), a grid of
/// `radius`-edged cubic cells, and the 27-cell neighbourhood (the cell
/// and its 26 face, edge and corner neighbours). A radius-edged cube
/// makes the 3×3×3 block a superset of the radius ball — two units
/// within `radius` differ by at most `radius` on every axis, so their
/// cell indices by at most one — and the exact squared 3D distance test
/// decides. Unlike [`VisionGrid2`], height separates: a unit directly
/// above another, farther than `radius`, is out of sight.
pub struct VisionGrid3<P> {
    radius: f32,
    /// `radius²`, computed once (the per-pair test compares against it).
    radius2: f32,
    _pos: PhantomData<fn() -> P>,
}

impl<P> VisionGrid3<P> {
    /// A vision grid of `radius` world units, clamped to a sane minimum
    /// (as [`VisionGrid2::new`]).
    #[must_use]
    pub fn new(radius: f32) -> Self {
        let radius = radius.max(1.0);
        Self {
            radius,
            radius2: radius * radius,
            _pos: PhantomData,
        }
    }

    /// The (clamped) vision radius.
    pub fn radius(&self) -> f32 {
        self.radius
    }
}

impl<P: Component + Copy + Spatial<Coord = f32>> Vision for VisionGrid3<P> {
    type Pos = P;
    type Cell = Cell3;

    #[inline]
    fn cell(&self, pos: &P) -> Cell3 {
        let [x, y, z] = pos.spatial();
        Cell3(
            (x / self.radius).floor() as i32,
            (y / self.radius).floor() as i32,
            (z / self.radius).floor() as i32,
        )
    }

    /// The 3×3×3 block, in a fixed order (third axis outermost).
    #[inline]
    fn neighborhood(&self, cell: Cell3) -> impl Iterator<Item = Cell3> {
        (-1..=1).flat_map(move |dz| {
            (-1..=1).flat_map(move |dy| {
                (-1..=1).map(move |dx| Cell3(cell.0 + dx, cell.1 + dy, cell.2 + dz))
            })
        })
    }

    #[inline]
    fn sees(&self, viewer: &P, target: &P) -> bool {
        let [vx, vy, vz] = viewer.spatial();
        let [tx, ty, tz] = target.spatial();
        let (dx, dy, dz) = (vx - tx, vy - ty, vz - tz);
        dx * dx + dy * dy + dz * dz <= self.radius2
    }
}
