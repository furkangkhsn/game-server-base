//! k0 spike — the ECS-free core of the kit's AOI cell-delta path
//! (`docs/KIT-ECS.md` §5, §8).
//!
//! What moved here from `gsb-kit` is the bookkeeping (`CellBook`), the
//! cell space (`Grid2`), the identity (`WireId` / `Minter`) and the packet
//! assembly. What did NOT move is every ECS read: the core never names a
//! world, an entity or a component. It sees
//!
//! - an opaque [`Owner`] (the s0b request design's idea, SERVICES §5.5:
//!   a binding converts its entity to and from a `u64`),
//! - plain `(owner, wire id, wire value)` observations, pushed by a
//!   binding's [`CellFeed`],
//! - and explicit removals (`(owner, still alive)`).
//!
//! The order a binding observes records in is the ECS's (archetype
//! order); the core makes the bytes independent of it by sorting each
//! cell's change list and each full piece by wire id
//! ([`CellBook::canonicalize`]) — so two bindings over the same scripted
//! world produce the same packets byte for byte.

mod book;
mod feed;
mod identity;
mod owner;
mod packet;
mod space;
mod wire;

pub use book::{CellBook, CellChanges, TouchInfo};
pub use feed::{CellFeed, Gone, run_tick};
pub use identity::{Minter, WireId};
pub use owner::Owner;
pub use packet::assemble_group_packet;
pub use space::{Cell, CellSpace, Grid2, Planar};
pub use wire::{ToWire, WireValue};
