//! [`Grid3`], the volumetric AOI preset (BACKLOG A5): the cell of a
//! wire value on three axes, the 27-cell view in its fixed order, and
//! the three-index `CellExit` body — read through [`Spatial`], so height
//! moves a cell (where [`Grid2`] over the same type ignores it).

use std::collections::HashSet;

use bytes::BytesMut;

use super::*;
use crate::testing::CellExit3;

fn cell(grid: &Grid3, w: &Wire3) -> Cell3 {
    CellSpace::<Wire3>::cell_of(grid, w)
}

/// The floor of (wire / cell size) on every axis — negative values
/// round down, a boundary value belongs to the upper cell — and the
/// second axis (height, for this game) moves the cell, where the planar
/// preset over the same wire does not.
#[test]
fn grid3_cells_floor_every_axis() {
    let grid = Grid3::new(20.0);
    assert_eq!(cell(&grid, &wire(5, 0, 45)), Cell3(0, 0, 2));
    assert_eq!(cell(&grid, &wire(5, 45, 0)), Cell3(0, 2, 0), "height");
    assert_eq!(cell(&grid, &wire(-1, -20, -21)), Cell3(-1, -1, -2));
    assert_eq!(cell(&grid, &wire(19, 20, 39)), Cell3(0, 1, 1), "boundary");
    let flat = Grid2::new(20.0);
    assert_eq!(
        CellSpace::<Wire3>::cell_of(&flat, &wire(5, 45, 0)),
        Cell(0, 0),
        "the planar preset ignores height"
    );
    assert_eq!(
        cell(&Grid3::new(0.0), &wire(1, 1, 1)),
        Cell3(2, 2, 2),
        "the cell size is clamped to 0.5, as Grid2's"
    );
}

/// The view is the 27 distinct cells of the 3×3×3 block, third axis
/// outermost (the packet's assembly order is fixed); two cells away on
/// any one axis is outside.
#[test]
fn grid3_view_is_the_27_cell_block_in_a_fixed_order() {
    let grid = Grid3::new(20.0);
    let view: Vec<Cell3> = CellSpace::<Wire3>::view(&grid, Cell3(4, -2, 0)).collect();
    assert_eq!(view.len(), 27);
    assert_eq!(view.iter().collect::<HashSet<_>>().len(), 27, "distinct");
    assert_eq!(view[0], Cell3(3, -3, -1), "first: the lowest corner");
    assert_eq!(view[1], Cell3(4, -3, -1), "the first axis innermost");
    assert_eq!(view[3], Cell3(3, -2, -1), "then the second");
    assert_eq!(view[9], Cell3(3, -3, 0), "the third outermost");
    assert_eq!(view[13], Cell3(4, -2, 0), "the cell itself in the middle");
    assert_eq!(view[26], Cell3(5, -1, 1));
    for far in [Cell3(6, -2, 0), Cell3(4, 0, 0), Cell3(4, -2, 2)] {
        assert!(!view.contains(&far), "{far:?} is two cells away");
    }
}

/// The `CellExit` body carries the three indices (tags 1, 2, 3; sint32)
/// and omits a zero one, as proto3 does — the origin cell is empty.
#[test]
fn grid3_cell_exit_body_carries_three_indices() {
    let grid = Grid3::new(20.0);
    let body = |c: Cell3| {
        let mut out = BytesMut::new();
        CellSpace::<Wire3>::encode_cell(&grid, c, &mut out);
        out
    };
    let decoded = <CellExit3 as prost::Message>::decode(body(Cell3(-3, 0, 7)).as_ref());
    assert_eq!(
        decoded.expect("a CellExit3"),
        CellExit3 { x: -3, y: 0, z: 7 }
    );
    assert_eq!(body(Cell3(-3, 0, 7)).as_ref(), &[0x08, 0x05, 0x18, 0x0e]);
    assert_eq!(body(Cell3(0, 1, 0)).as_ref(), &[0x10, 0x02]);
    assert!(body(Cell3(0, 0, 0)).is_empty());
}
