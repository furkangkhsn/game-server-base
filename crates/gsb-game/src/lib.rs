//! Demo game logic for gsb.
//!
//! This crate is the *only* place that knows anything about a particular
//! game. It plugs into the core via the [`gsb_core::room::GameLogic`]
//! supertrait — the single-source shared contract (see
//! `docs/TRAIT-ARCHITECTURE.md`) — narrowed by [`gsb_core::room::RoomLogic`]
//! with the room-exclusive request/result seams (see [`room::OpenRoom`]),
//! and it registers its wire messages with the
//! [`gsb_protocol::MessageTable`] (see [`register`]).
//!
//! Everything here is intentionally small and replaceable: swap the
//! components, systems, and `GameLogic` implementation for a real MOBA or
//! MMORPG without touching the core, net, protocol, or ecs crates.
//!
//! The demo game has four interchangeable rooms — the same
//! components, the same movement system, the same wire format, and only
//! the *visibility strategy* differs (selected in `gsb-server`'s config,
//! see `docs/DESIGN.md` §8):
//!
//! - [`room::OpenRoom`] — `GroupKey = ()`: everyone sees the whole world
//!   (the baseline, no grouping).
//! - [`aoi::AoiRoom`] — `GroupKey = Cell`: spatial AOI (3×3 cell block).
//! - [`team::TeamRoom`] — `GroupKey = Team`: team fog of war (2 groups;
//!   `group_of` looks at game state, not position).
//! - [`pvs::SectorRoom`] — `GroupKey = Sector`: per-map-segment PVS
//!   (static visibility table over hand-authored convex sectors).
//!
//! The fifth strategy is a different *topology*, not just a group key:
//! [`sharded::ShardedRoom`] partitions the world into a grid of shards
//! (each shard its own actor with its own `World`), implementing
//! [`GameLogic`](gsb_core::room::GameLogic) plus the sharding seam of
//! [`gsb_core::shard::ShardLogic`] — see `docs/DESIGN.md` for the
//! sharding design and the wire-id range partitioning.
//!
//! Everything the rooms share that is *not* a visibility-strategy
//! decision (identity minting, the connection table, input ingestion, the
//! system run, orphan stamping) lives in [`common`], once.

mod demo;
mod kit;

pub mod pvs;
pub mod sharded;
pub mod team;

// ── Compatibility paths (phase 0) ─────────────────────────────────────────
//
// The crate's public API predates the kit/demo split: every consumer
// (`gsb-server`'s factories and config, the load generator, the tests,
// the examples) names items by these paths. Phase 0 moves the code, not
// the paths — these re-exports keep every old path resolving to the same
// item until the crate split (phase 2) defines the new public surface.

pub use demo::{economy, game, op, register, systems};
pub use kit::DEFAULT_DISCONNECT_GRACE;
pub use kit::aoi;

/// The open-visibility room and the demo's spawn distribution
/// (compatibility path).
pub mod room {
    pub use crate::demo::spawn::{DEFAULT_SPAWN_HALF, spawn_pos};
    pub use crate::kit::room::OpenRoom;
}

/// ECS components of the demo game (compatibility path: the demo's
/// components plus the kit-owned [`WireId`](components::WireId)).
pub mod components {
    pub use crate::demo::components::{DEFAULT_SPEED, MoveTarget, Position, Speed};
    pub use crate::kit::identity::WireId;
}
