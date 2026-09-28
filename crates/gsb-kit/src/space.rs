//! [`CellSpace`] — the AOI's space over WIRE values (KIT-ARCHITECTURE
//! §4.2), and [`Grid2`], its 2D grid preset (§7); [`Planar`] and
//! [`Spatial`] — the accessors through which the 2D and 3D presets read
//! a game's position or wire type without knowing it.
//!
//! The AOI cell is computed from the wire value, not from the
//! simulation state: the client holds only wire values and must derive
//! the SAME cell to service a `CellExit` record (forget every entity it
//! holds for that cell). So the space is keyed by the codec's `Wire`.

use std::fmt::Debug;
use std::hash::Hash;

use bytes::BytesMut;

mod partition;
mod sectors;
mod vision;

#[cfg(test)]
mod tests;

pub use partition::{GridPartition2, Partition, grid_shape, shard_at};
pub use sectors::{ConvexSectors2, Sector, SectorMap};
pub use vision::{MAX_SIGHT_CELLS, Vision, VisionGrid2, VisionGrid3};

/// Where a value lies on the ground plane — the accessor the kit's 2D
/// presets read a game's types through (§7): [`Grid2`] reads the codec's
/// wire value (`Coord = i32`), the simulation presets read the
/// position component (`Coord = f32`).
///
/// The projection is the GAME's choice, made once per type: a top-down
/// 2D game answers `[x, y]`; a 3D game whose world lives on the ground
/// plane answers `[x, z]` for its `Pos3 { x, y, z }` and for its 3D wire
/// value, and then uses the planar presets unchanged. A preset that
/// needs all three axes ([`VisionGrid3`]) reads [`Spatial`] instead; a
/// type can implement both.
///
/// **The unit.** The projection reports the value in a unit of the
/// game's choosing, but a preset that reads BOTH a position and a wire
/// value compares them in one unit: [`GridPartition2`] tests a
/// neighbour's wire value against region rectangles laid out in the
/// position's unit. So a game's position `Planar` and wire `Planar` must
/// report the SAME unit — a wire quantized finer than the position
/// (centimetres over metres) projects back to the position's unit (see
/// [`GridPartition2`]; debug builds check it). A preset reading only one
/// of the two ([`Grid2`]: the wire; the vision and sector presets: the
/// position) takes its own parameters in that value's unit.
pub trait Planar {
    /// The coordinate type: `i32` for a quantized wire value, `f32` for
    /// a simulation position.
    type Coord: Copy;

    /// The value's two ground-plane coordinates, in a fixed axis order
    /// (the first is the grid's column axis, the second its row axis).
    fn planar(&self) -> [Self::Coord; 2];
}

/// Where a value lies in 3D space — the accessor the kit's 3D presets
/// read a game's types through (§7; today [`VisionGrid3`]), the
/// three-axis sibling of [`Planar`].
///
/// The axis order is the game's (the 3D presets are isotropic: a
/// uniform radius, cubic cells). A 3D game typically implements both
/// accessors on its position: `Spatial` for true 3D presets (the arena's
/// team vision, where height matters) and `Planar` (`[x, z]`) for the
/// ground-plane ones (an MMO's AOI and shard grid).
pub trait Spatial {
    /// The coordinate type: `f32` for a simulation position.
    type Coord: Copy;

    /// The value's three coordinates, in a fixed axis order.
    fn spatial(&self) -> [Self::Coord; 3];
}

/// A cubic cell of a 3D grid — [`VisionGrid3`]'s cell key. Cell indices
/// are the floor of (position / cell edge) on each axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Cell3(pub i32, pub i32, pub i32);

/// A cell partition of the wire-value space: the AOI group key, the
/// visibility neighbourhood, and the body of the `CellExit` record.
pub trait CellSpace<W>: Send + 'static {
    /// The cell key (the AOI room's `GroupKey`). `Default` is the key a
    /// connection without an entity is grouped under.
    type Cell: Eq + Hash + Copy + Debug + Default + Send + 'static;

    /// The cell containing wire value `wire`.
    fn cell_of(&self, wire: &W) -> Self::Cell;

    /// The cells a member of `cell` sees, in a FIXED order (the assembly
    /// order of the group's packet — deterministic bytes). 2D: the 3×3
    /// block; 3D: 27 cells.
    fn view(&self, cell: Self::Cell) -> impl Iterator<Item = Self::Cell>;

    /// Append the BODY of the `CellExit` record for `cell` to `out`; the
    /// kit writes the `cell_exits` field's tag and length around it.
    fn encode_cell(&self, cell: Self::Cell, out: &mut BytesMut);
}

/// A spatial cell of the 2D grid — [`Grid2`]'s cell key. Cell indices
/// are the floor of (wire position / `cell_size`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Cell(pub i32, pub i32);

/// How many cells the visibility block extends in each direction from
/// the player's cell. `1` ⇒ a 3×3 block (the cell + its 8 ring-1
/// neighbors).
pub(crate) const RADIUS: i32 = 1;

/// The (dx, dy) offsets of the visibility block centered on a cell
/// (deterministic order: the assembly order of the group's packet).
pub(crate) const BLOCK_OFFSETS: [(i32, i32); 9] = [
    (-RADIUS, -RADIUS),
    (0, -RADIUS),
    (RADIUS, -RADIUS),
    (-RADIUS, 0),
    (0, 0),
    (RADIUS, 0),
    (-RADIUS, RADIUS),
    (0, RADIUS),
    (RADIUS, RADIUS),
];

/// The cell containing the WIRE (integer) position: `floor(x / cell_size)`
/// on the integer coordinates, so the client — which holds only wire
/// coordinates — computes the same cell.
#[inline]
pub(crate) fn cell_of(x: i32, y: i32, cell_size: f32) -> Cell {
    Cell(
        (x as f32 / cell_size).floor() as i32,
        (y as f32 / cell_size).floor() as i32,
    )
}

/// The 2D grid preset: square cells of `cell_size` wire units over any
/// wire value with an integer ground-plane projection ([`Planar`] with
/// `Coord = i32`), a 3×3 view, and a `CellExit` body of
/// `{ sint32 x = 1; sint32 y = 2; }` (the cell's two plane indices;
/// proto3: a zero index is omitted).
///
/// `cell_size` is the one tunable: it must keep a cell's 3×3 block under
/// the snapshot byte budget at the expected peak density, and it sets
/// the visibility leak band (≤ one cell edge).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid2 {
    cell_size: f32,
}

impl Grid2 {
    /// A grid of `cell_size` wire units per cell edge, clamped to a sane
    /// minimum so a degenerate `0` cannot produce a single infinite cell.
    #[must_use]
    pub fn new(cell_size: f32) -> Self {
        Self {
            cell_size: cell_size.max(0.5),
        }
    }
}

impl<W: Planar<Coord = i32>> CellSpace<W> for Grid2 {
    type Cell = Cell;

    #[inline]
    fn cell_of(&self, wire: &W) -> Cell {
        let [x, y] = wire.planar();
        cell_of(x, y, self.cell_size)
    }

    #[inline]
    fn view(&self, cell: Cell) -> impl Iterator<Item = Cell> {
        BLOCK_OFFSETS
            .into_iter()
            .map(move |(dx, dy)| Cell(cell.0 + dx, cell.1 + dy))
    }

    fn encode_cell(&self, cell: Cell, out: &mut BytesMut) {
        if cell.0 != 0 {
            prost::encoding::sint32::encode(1, &cell.0, out);
        }
        if cell.1 != 0 {
            prost::encoding::sint32::encode(2, &cell.1, out);
        }
    }
}
