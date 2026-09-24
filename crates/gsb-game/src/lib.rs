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
//! decision (identity minting, the connection table, the input seq/ack
//! rule, the system run, orphan stamping, the cell-delta engine, park
//! bookkeeping) lives once, in the kit's `common` module.
//!
//! ## Layout: `kit` and `demo` (KIT-ARCHITECTURE §10, phase 0)
//!
//! The source is split in two private modules ahead of the crate split
//! (`gsb-kit` / `gsb-demo`, phase 2):
//!
//! - `kit` — the reusable strategies and machinery: the five rooms, the
//!   cell-delta engine, park/resume, the input sequence rule, wire
//!   identity, the snapshot/`Private` framing.
//! - `demo` — the example game: components, movement, the wire messages
//!   and opcodes, the spawn distribution, input decoding, the bot, the
//!   RPC handlers and the economy service, the PVS map.
//!
//! The kit's seams (`RecordCodec`, `CellSpace`, `Game`) exist since
//! phase 1a and the demo implements them (`DemoGame`, `DemoCodec`, the
//! kit's `Grid2` preset); the open and AOI rooms are generic over them.
//! The remaining rooms still call into the demo; every such call goes
//! through ONE module, `kit::seam`, whose contents are the phase-1b work
//! list. A unit test (`layering`) locks the rule. The public paths below
//! are unchanged (the converted rooms' old names are type aliases onto
//! the demo instantiation).

mod demo;
mod kit;

#[cfg(test)]
mod layering;

// ── Compatibility paths (phase 0) ─────────────────────────────────────────
//
// The crate's public API predates the kit/demo split: every consumer
// (`gsb-server`'s factories and config, the load generator, the tests,
// the examples) names items by these paths. Phase 0 moves the code, not
// the paths — these re-exports keep every old path resolving to the same
// item until the crate split (phase 2) defines the new public surface.

pub use demo::{economy, game, op, register, systems};
pub use kit::DEFAULT_DISCONNECT_GRACE;

/// The team-fog room, its group key and membership component
/// (compatibility path; the strategy's design notes live on the kit's
/// generic [`TeamRoom`](crate::kit::team::TeamRoom)).
pub mod team {
    pub use crate::kit::team::{DEFAULT_VISION_RADIUS, TEAM_COUNT, Team, TeamMember};

    /// The team-fog room running the demo game over the kit's 2D vision
    /// preset: the kit's generic [`TeamRoom`](crate::kit::team::TeamRoom)
    /// instantiated with the demo's `Game` and `VisionGrid2<Position>`
    /// (constructors: `new`, `with_spawn_half`, `with_disconnect_grace`).
    pub type TeamRoom = crate::kit::team::TeamRoom<
        crate::demo::play::DemoGame,
        crate::kit::space::VisionGrid2<crate::demo::components::Position>,
    >;
}

/// The AOI room and its cell key (compatibility path; the strategy's
/// design notes live on the kit's generic
/// [`AoiRoom`](crate::kit::aoi::AoiRoom)).
pub mod aoi {
    pub use crate::kit::space::Cell;

    /// The AOI room running the demo game over the kit's 2D grid preset:
    /// the kit's generic [`AoiRoom`](crate::kit::aoi::AoiRoom)
    /// instantiated with the demo's `Game` and `Grid2` (constructors:
    /// `new`, `with_spawn_half`, `with_disconnect_grace`).
    pub type AoiRoom =
        crate::kit::aoi::AoiRoom<crate::demo::play::DemoGame, crate::kit::space::Grid2>;
}

/// The PVS room and the demo map's sector key (compatibility path).
pub mod pvs {
    pub use crate::demo::sectors::{SECTOR_OUT, Sector};
    pub use crate::kit::pvs::SectorRoom;
}

/// The sharded rooms, their grid partition and migration state, and the
/// demo's border-strip payload (compatibility path).
pub mod sharded {
    pub use crate::demo::wire::StripPos;
    pub use crate::kit::sharded::{
        ShardParkRecord, ShardedRoom, ShardedRoomState, ShardedSpatialRoom, grid_shape, shard_at,
    };
}

/// The open-visibility room and the demo's spawn distribution
/// (compatibility path).
pub mod room {
    pub use crate::demo::spawn::{DEFAULT_SPAWN_HALF, spawn_pos};

    /// The open-visibility room running the demo game: the kit's generic
    /// [`OpenRoom`](crate::kit::room::OpenRoom) instantiated with the
    /// demo's `Game` (constructors: `new`, `with_spawn_half`,
    /// `with_economy`, `with_disconnect_grace`).
    pub type OpenRoom = crate::kit::room::OpenRoom<crate::demo::play::DemoGame>;
}

/// ECS components of the demo game (compatibility path: the demo's
/// components plus the kit-owned [`WireId`](components::WireId)).
pub mod components {
    pub use crate::demo::components::{DEFAULT_SPEED, MoveTarget, Position, Speed};
    pub use crate::kit::identity::WireId;
}
