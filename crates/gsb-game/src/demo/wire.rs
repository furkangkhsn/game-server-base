//! The demo's wire value of one entity: its truncated position — what
//! the snapshot records carry and what crosses a shard seam in the border
//! strip (the future `RecordCodec::Wire`, KIT-ARCHITECTURE §4.1: the
//! sharded `Strip` IS the game's wire value).

/// The demo's visibility-strip payload
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
