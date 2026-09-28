//! [`Vision`] — the team-fog seam over SIMULATION positions
//! (KIT-ARCHITECTURE §4.2), and its presets (§7): [`VisionGrid2`] on the
//! ground plane, [`VisionGrid3`] in 3D.
//!
//! Team vision is a distance test between units, so it reads the
//! simulation position (not the wire value the AOI space reads). The
//! seam is two things: the exact test ([`Vision::sees`]) and a
//! neighbourhood grid that bounds which units the test has to be run
//! against ([`Vision::cell`] / [`Vision::neighborhood`]) — the room
//! only ever compares units whose cells are neighbours. Per-unit sight
//! (BACKLOG A8, opt-in) adds the test with a unit's own radius
//! ([`Vision::sees_within`]) and the neighbourhood widened to the
//! largest such radius ([`Vision::neighborhood_within`]).

use std::hash::Hash;
use std::marker::PhantomData;

use bevy_ecs::component::Component;

use crate::space::{BLOCK_OFFSETS, Cell, Cell3, Planar, Spatial};

mod sight;

pub use sight::MAX_SIGHT_CELLS;
use sight::{clamp_sight, rings};

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

    /// Whether a unit at `viewer` with its OWN sight radius `radius`
    /// (the game's [`SightRadius`](crate::team::SightRadius), BACKLOG
    /// A8) sees a unit at `target`. The rooms call it only for a source
    /// that carries a radius; every other source goes through
    /// [`Self::sees`].
    ///
    /// The default ignores `radius` (a model without per-unit sight:
    /// [`Self::sees`] is its whole rule); the kit's presets override it.
    fn sees_within(&self, viewer: &Self::Pos, radius: f32, target: &Self::Pos) -> bool {
        let _ = radius;
        self.sees(viewer, target)
    }

    /// The cells whose units — the model's own sources and sources with
    /// a sight radius up to `reach` — may see into `cell`, in a fixed
    /// order. **The widened grid contract:** it contains
    /// [`Self::neighborhood`]`(cell)`, and whenever `sees_within(viewer,
    /// r, target)` holds with `r ≤ reach`, the viewer's cell is in
    /// `neighborhood_within(cell(target), reach)`. The rooms call it
    /// only for a team with at least one such source this tick.
    ///
    /// The default is [`Self::neighborhood`] (consistent with the default
    /// [`Self::sees_within`]).
    fn neighborhood_within(
        &self,
        cell: Self::Cell,
        reach: f32,
    ) -> impl Iterator<Item = Self::Cell> {
        let _ = reach;
        self.neighborhood(cell)
    }
}

/// The 2D vision preset: a uniform vision `radius` on the ground plane
/// (any position component with an `f32` [`Planar`] projection), a grid
/// of `radius`-sized cells, and the 3×3 neighbourhood. A radius-sized
/// cell makes the 3×3 block a superset of the radius disk (a corner
/// cell can hold units up to `radius · √2` away), so the exact
/// squared-distance test decides. A unit's own radius (A8) uses the same
/// inclusive test and the `(2k + 1)²` block around the target
/// ([`MAX_SIGHT_CELLS`]).
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

    /// The same squared-distance test against the unit's own (clamped)
    /// radius: the boundary is inclusive, as in [`Self::sees`].
    #[inline]
    fn sees_within(&self, viewer: &P, radius: f32, target: &P) -> bool {
        let r = clamp_sight(radius, self.radius);
        let [vx, vy] = viewer.planar();
        let [tx, ty] = target.planar();
        let (dx, dy) = (vx - tx, vy - ty);
        dx * dx + dy * dy <= r * r
    }

    /// The `(2k + 1)²` block, `k = ⌈reach / radius⌉` clamped to 1 ..=
    /// [`MAX_SIGHT_CELLS`] (the 3×3 block for a reach up to the radius).
    #[inline]
    fn neighborhood_within(&self, cell: Cell, reach: f32) -> impl Iterator<Item = Cell> {
        let k = rings(reach, self.radius);
        (-k..=k).flat_map(move |dy| (-k..=k).map(move |dx| Cell(cell.0 + dx, cell.1 + dy)))
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
/// above another, farther than `radius`, is out of sight. A unit's own
/// radius (A8): as [`VisionGrid2`], with the `(2k + 1)³` block.
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

    /// As [`VisionGrid2::sees_within`], in 3D.
    #[inline]
    fn sees_within(&self, viewer: &P, radius: f32, target: &P) -> bool {
        let r = clamp_sight(radius, self.radius);
        let [vx, vy, vz] = viewer.spatial();
        let [tx, ty, tz] = target.spatial();
        let (dx, dy, dz) = (vx - tx, vy - ty, vz - tz);
        dx * dx + dy * dy + dz * dz <= r * r
    }

    /// The `(2k + 1)³` block, `k` as in [`VisionGrid2::neighborhood_within`].
    #[inline]
    fn neighborhood_within(&self, cell: Cell3, reach: f32) -> impl Iterator<Item = Cell3> {
        let k = rings(reach, self.radius);
        (-k..=k).flat_map(move |dz| {
            (-k..=k).flat_map(move |dy| {
                (-k..=k).map(move |dx| Cell3(cell.0 + dx, cell.1 + dy, cell.2 + dz))
            })
        })
    }
}
