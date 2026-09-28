//! [`Grid3`] — the volumetric preset of [`CellSpace`] (KIT-ARCHITECTURE
//! §7, BACKLOG A5): cubic cells over any wire value with an integer
//! [`Spatial`] projection, the 27-cell view, and a three-index
//! `CellExit` body. [`Grid2`](super::Grid2)'s contract on three axes.

use bytes::BytesMut;

use super::{Cell, Cell3, CellSpace, Spatial, cell_of};

/// The (dx, dy, dz) offsets of the 3×3×3 view centred on a cell, in a
/// fixed order — the third axis outermost, then the second, then the
/// first (the order [`VisionGrid3`](super::VisionGrid3) walks its block
/// in; the assembly order of the group's packet).
const VIEW_OFFSETS: [(i32, i32, i32); 27] = view_offsets();

const fn view_offsets() -> [(i32, i32, i32); 27] {
    let mut out = [(0, 0, 0); 27];
    let mut i = 0;
    while i < 27 {
        let (dx, dy, dz) = (i % 3, (i / 3) % 3, i / 9);
        out[i] = (dx as i32 - 1, dy as i32 - 1, dz as i32 - 1);
        i += 1;
    }
    out
}

/// The volumetric grid preset: cubic cells of `cell_size` wire units
/// over any wire value with an integer projection in space
/// ([`Spatial`] with `Coord = i32`), the 27-cell view (the cell and its
/// 26 face, edge and corner neighbours), and a `CellExit` body of `{
/// sint32 x = 1; sint32 y = 2; sint32 z = 3; }` (the cell's three
/// indices in the projection's axis order; proto3: a zero index is
/// omitted). A client derives the cell the same way: the floor of
/// (wire coordinate / `cell_size`) on every axis.
///
/// Opt-in: a game whose interest is on the ground plane keeps
/// [`Grid2`](super::Grid2) (its bytes, its 3×3 view). `Grid3` is for a
/// game where height separates interest — a space game, a flying game, a
/// tall building whose floors should not see each other. The one
/// tunable is `cell_size`, as in 2D, but the view is 27 cells instead of
/// 9: a group's packet assembles three times as many pieces, so the cell
/// must keep a 3×3×3 block under the snapshot byte budget at the
/// expected peak density; the visibility leak band is at most one cell
/// edge on every axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid3 {
    cell_size: f32,
}

impl Grid3 {
    /// A grid of `cell_size` wire units per cube edge, clamped to the
    /// same minimum as [`Grid2::new`](super::Grid2::new).
    #[must_use]
    pub fn new(cell_size: f32) -> Self {
        Self {
            cell_size: cell_size.max(0.5),
        }
    }
}

impl<W: Spatial<Coord = i32>> CellSpace<W> for Grid3 {
    type Cell = Cell3;

    /// [`Grid2`](super::Grid2)'s formula on the first two axes, and the
    /// same on the third.
    #[inline]
    fn cell_of(&self, wire: &W) -> Cell3 {
        let [x, y, z] = wire.spatial();
        let Cell(cx, cy) = cell_of(x, y, self.cell_size);
        Cell3(cx, cy, (z as f32 / self.cell_size).floor() as i32)
    }

    #[inline]
    fn view(&self, cell: Cell3) -> impl Iterator<Item = Cell3> {
        VIEW_OFFSETS
            .into_iter()
            .map(move |(dx, dy, dz)| Cell3(cell.0 + dx, cell.1 + dy, cell.2 + dz))
    }

    fn encode_cell(&self, cell: Cell3, out: &mut BytesMut) {
        for (tag, index) in [(1, cell.0), (2, cell.1), (3, cell.2)] {
            if index != 0 {
                prost::encoding::sint32::encode(tag, &index, out);
            }
        }
    }
}
