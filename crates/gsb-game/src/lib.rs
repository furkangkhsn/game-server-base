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
//! - [`team::TeamRoom`] — `GroupKey = Team`: team fog of war (one group
//!   per team — the demo assigns 2;
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
    pub use crate::demo::spawn::TEAM_COUNT;
    pub use crate::kit::team::{DEFAULT_VISION_RADIUS, Team, TeamMember};

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

/// The PVS room and the demo map's sector key (compatibility path; the
/// strategy's design notes live on the kit's generic
/// [`SectorRoom`](crate::kit::pvs::SectorRoom)).
pub mod pvs {
    pub use crate::demo::sectors::SECTOR_OUT;
    pub use crate::kit::space::Sector;

    /// The PVS room running the demo game over its four-sector map in the
    /// kit's convex-sector preset (constructors: `new`, `with_spawn_half`,
    /// `with_disconnect_grace`).
    pub type SectorRoom = crate::kit::pvs::SectorRoom<
        crate::demo::play::DemoGame,
        crate::kit::space::ConvexSectors2<crate::demo::components::Position>,
    >;
}

/// The sharded rooms, their grid partition and migration state, and the
/// demo's border-strip payload (compatibility path; the strategies'
/// design notes live on the kit's generic
/// [`ShardedRoom`](crate::kit::sharded::ShardedRoom)).
pub mod sharded {
    pub use crate::demo::migrate::DemoMig;
    pub use crate::demo::wire::StripPos;
    pub use crate::kit::sharded::{KitMig, ShardParkRecord};
    pub use crate::kit::space::{grid_shape, shard_at};

    /// One shard of the demo's sharded world: the kit's generic
    /// [`ShardedRoom`](crate::kit::sharded::ShardedRoom) instantiated with
    /// the demo's `Game` over the kit's 2D grid partition (constructors:
    /// `new`, `with_disconnect_grace`, `with_economy`).
    pub type ShardedRoom = crate::kit::sharded::ShardedRoom<
        crate::demo::play::DemoGame,
        crate::kit::space::GridPartition2<crate::demo::components::Position>,
    >;

    /// One shard of the demo's sharded × spatial world: the kit's generic
    /// [`ShardedSpatialRoom`](crate::kit::sharded::ShardedSpatialRoom)
    /// over the demo shard and the kit's 2D grid AOI (constructors: `new`,
    /// `with_disconnect_grace`, `with_economy`).
    pub type ShardedSpatialRoom = crate::kit::sharded::ShardedSpatialRoom<
        crate::demo::play::DemoGame,
        crate::kit::space::GridPartition2<crate::demo::components::Position>,
        crate::kit::space::Grid2,
    >;

    /// The demo's migrating entity state: the demo's captured components
    /// in the kit's envelope (`KitMig` — the park record rides along).
    pub type ShardedRoomState = KitMig<DemoMig>;
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
