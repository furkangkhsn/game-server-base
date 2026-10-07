//! The cell space over WIRE values — `gsb-kit`'s `CellSpace` / `Grid2`,
//! unchanged in substance (it never touched the ECS; only the sibling
//! `Vision` / `Partition` / `SectorMap` traits bound their position on
//! `bevy_ecs::Component`, which the agnostic kit drops for `Copy`).

use std::fmt::Debug;
use std::hash::Hash;

use crate::wire::zigzag;

/// Where a value lies on the ground plane.
pub trait Planar {
    /// The value's two ground-plane coordinates.
    fn planar(&self) -> [i32; 2];
}

/// A cell partition of the wire-value space.
pub trait CellSpace<W> {
    /// The cell key (the AOI group key).
    type Cell: Eq + Hash + Copy + Debug + Ord + Default;
    /// The cell containing wire value `wire`.
    fn cell_of(&self, wire: &W) -> Self::Cell;
    /// The cells a member of `cell` sees, in a fixed order.
    fn view(&self, cell: Self::Cell) -> impl Iterator<Item = Self::Cell>;
    /// The body of the `CellExit` record for `cell`.
    fn encode_cell(&self, cell: Self::Cell, out: &mut Vec<u8>);
}

/// A 2D grid cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Cell(pub i32, pub i32);

const BLOCK_OFFSETS: [(i32, i32); 9] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (0, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// The 2D grid preset: square cells, a 3×3 view.
#[derive(Debug, Clone, Copy)]
pub struct Grid2 {
    cell_size: f32,
}

impl Grid2 {
    /// A grid of `cell_size` wire units per cell edge.
    #[must_use]
    pub fn new(cell_size: f32) -> Self {
        Self {
            cell_size: cell_size.max(0.5),
        }
    }
}

impl<W: Planar> CellSpace<W> for Grid2 {
    type Cell = Cell;

    #[inline]
    fn cell_of(&self, wire: &W) -> Cell {
        let [x, y] = wire.planar();
        Cell(
            (x as f32 / self.cell_size).floor() as i32,
            (y as f32 / self.cell_size).floor() as i32,
        )
    }

    fn view(&self, cell: Cell) -> impl Iterator<Item = Cell> {
        BLOCK_OFFSETS
            .into_iter()
            .map(move |(dx, dy)| Cell(cell.0 + dx, cell.1 + dy))
    }

    fn encode_cell(&self, cell: Cell, out: &mut Vec<u8>) {
        zigzag(cell.0, out);
        zigzag(cell.1, out);
    }
}
