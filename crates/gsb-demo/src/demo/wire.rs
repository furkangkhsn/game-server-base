//! The demo's wire value of one entity: its truncated position — what
//! the snapshot records carry and what crosses a shard seam in the border
//! strip (`RecordCodec::Wire`, KIT-ARCHITECTURE §4.1: the sharded
//! `Strip` IS the game's wire value).

use gsb_kit::space::Planar;

/// The demo's wire value ([`DemoCodec`](crate::demo::codec::DemoCodec)'s
/// `Wire`) and, identically, its visibility-strip payload
/// ([`GameLogic::Strip`](gsb_core::room::GameLogic::Strip)): the
/// entity's TRUNCATED position — exactly the content the core-fixed
/// boundary record carried before generalization, so this round changes
/// no wire bytes. A game needing more across the seam extends THIS type
/// (velocity, facing, hp snapshot); the core never learns about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripPos {
    pub x: i32,
    pub y: i32,
}

/// The wire value on the ground plane (the kit's `Grid2` AOI cells are
/// computed from it — the client derives the same cell from the same
/// integers).
impl Planar for StripPos {
    type Coord = i32;

    #[inline]
    fn planar(&self) -> [i32; 2] {
        [self.x, self.y]
    }
}
