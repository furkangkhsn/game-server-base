//! The example game for gsb-kit (KIT-ARCHITECTURE §3: the "example
//! project" layer).
//!
//! This crate is the *only* place that knows anything about a particular
//! game. It implements the kit's seams (`gsb_kit::codec::RecordCodec`,
//! `gsb_kit::game::{Game, TeamGame, ShardGame}`, and `gsb_kit::space::Planar`
//! for its position and wire types — `DemoGame`, `DemoCodec`), plugs the
//! kit's generic rooms together with it, and registers its wire messages
//! with the [`gsb_protocol::MessageTable`] (see [`register`]). The rooms
//! are complete [`gsb_core::room::GameLogic`] implementations — the
//! kit's, over this game.
//!
//! Everything here is intentionally small and replaceable: a real MOBA or
//! MMORPG is another crate like this one, over the same kit, without
//! touching the core, net, protocol, ecs or kit crates.
//!
//! The demo game has four interchangeable rooms — the same components,
//! the same movement system, the same wire format, and only the
//! *visibility strategy* differs (selected in `gsb-server`'s config, see
//! `docs/DESIGN.md` §8):
//!
//! - [`room::OpenRoom`] — `GroupKey = ()`: everyone sees the whole world
//!   (the baseline, no grouping).
//! - [`aoi::AoiRoom`] — `GroupKey = Cell`: spatial AOI (3×3 cell block).
//! - [`team::TeamRoom`] — `GroupKey = Team`: team fog of war (one group
//!   per team — the demo assigns 2; `group_of` looks at game state, not
//!   position).
//! - [`pvs::SectorRoom`] — `GroupKey = Sector`: per-map-segment PVS
//!   (static visibility table over hand-authored convex sectors).
//!
//! The fifth strategy is a different *topology*, not just a group key:
//! [`sharded::ShardedRoom`] partitions the world into a grid of shards
//! (each shard its own actor with its own `World`), implementing
//! [`GameLogic`](gsb_core::room::GameLogic) plus the sharding seam of
//! [`gsb_core::shard::ShardLogic`] — see `docs/DESIGN.md` for the
//! sharding design and the interleaved wire-id minting.
//!
//! The rooms' constructors are extension traits over the kit's room types
//! ([`prelude`]; `use gsb_demo::prelude::*;`).
//!
//! The wire: `game.proto` holds the demo's own messages and its TYPED
//! mirrors of the kit's envelope (`WorldSnapshot`, `Private` — the kit
//! writes them with opaque record bodies, the demo's codec writes each
//! body); `tests/kit_wire.rs` pins the two definitions to identical
//! bytes.

mod demo;

// ── Compatibility paths (phase 0) ─────────────────────────────────────────
//
// The crate's public API predates the kit/demo split: every consumer
// (`gsb-server`'s factories and config, the load generator, the tests,
// the examples) names items by these paths, and the crate split
// (phase 2) kept them — the rooms are type aliases onto the kit's
// generic rooms instantiated with the demo game, and the kit-owned items
// (`WireId`, `Cell`, `Team`, …) are re-exported from `gsb_kit`.

/// The demo's [`Game`](gsb_kit::game::Game) itself, for a game that
/// composes it (the B21 lobby example wraps it to spawn by the ticket's
/// verified claims — `examples/lobby`).
pub use demo::play::DemoGame;
pub use demo::{economy, game, op, register, systems};
pub use gsb_kit::DEFAULT_DISCONNECT_GRACE;

/// The demo rooms' constructors (`OpenRoom::new()`,
/// `AoiRoom::with_spawn_half(..)`, `.with_economy(..)`, …): one
/// extension trait per room, since the room types are the kit's (see
/// `demo/rooms.rs` for why). `use gsb_demo::prelude::*;` brings them all
/// into scope.
pub mod prelude {
    pub use crate::demo::rooms::{
        AoiRoomExt, OpenRoomExt, SectorRoomExt, ShardedRoomExt, ShardedSpatialRoomExt, TeamRoomExt,
    };
}

/// The team-fog room, its group key and membership component
/// (compatibility path; the strategy's design notes live on the kit's
/// generic [`TeamRoom`](gsb_kit::team::TeamRoom)).
pub mod team {
    pub use crate::demo::rooms::TeamRoomExt;
    pub use crate::demo::spawn::TEAM_COUNT;
    pub use gsb_kit::team::{DEFAULT_VISION_RADIUS, Team, TeamMember};

    /// The team-fog room running the demo game over the kit's 2D vision
    /// preset: the kit's generic [`TeamRoom`](gsb_kit::team::TeamRoom)
    /// instantiated with the demo's `Game` and `VisionGrid2<Position>`
    /// (constructors: [`TeamRoomExt`]'s `new`, `with_spawn_half`,
    /// `with_economy`; the kit's `with_disconnect_grace`).
    pub type TeamRoom = gsb_kit::team::TeamRoom<
        crate::demo::play::DemoGame,
        gsb_kit::space::VisionGrid2<crate::demo::components::Position>,
    >;
}

/// The AOI room and its cell key (compatibility path; the strategy's
/// design notes live on the kit's generic
/// [`AoiRoom`](gsb_kit::aoi::AoiRoom)).
pub mod aoi {
    pub use crate::demo::rooms::AoiRoomExt;
    pub use gsb_kit::space::Cell;

    /// The AOI room running the demo game over the kit's 2D grid preset:
    /// the kit's generic [`AoiRoom`](gsb_kit::aoi::AoiRoom)
    /// instantiated with the demo's `Game` and `Grid2` (constructors:
    /// [`AoiRoomExt`]'s `new`, `with_spawn_half`, `with_economy`; the
    /// kit's `with_disconnect_grace`).
    pub type AoiRoom = gsb_kit::aoi::AoiRoom<crate::demo::play::DemoGame, gsb_kit::space::Grid2>;
}

/// The PVS room and the demo map's sector key (compatibility path; the
/// strategy's design notes live on the kit's generic
/// [`SectorRoom`](gsb_kit::pvs::SectorRoom)).
pub mod pvs {
    pub use crate::demo::rooms::SectorRoomExt;
    pub use crate::demo::sectors::SECTOR_OUT;
    pub use gsb_kit::space::Sector;

    /// The PVS room running the demo game over its four-sector map in the
    /// kit's convex-sector preset (constructors: [`SectorRoomExt`]'s
    /// `new`, `with_spawn_half`, `with_economy`; the kit's
    /// `with_disconnect_grace`).
    pub type SectorRoom = gsb_kit::pvs::SectorRoom<
        crate::demo::play::DemoGame,
        gsb_kit::space::ConvexSectors2<crate::demo::components::Position>,
    >;
}

/// The sharded rooms, their grid partition and migration state, and the
/// demo's border-strip payload (compatibility path; the strategies'
/// design notes live on the kit's generic
/// [`ShardedRoom`](gsb_kit::sharded::ShardedRoom)).
pub mod sharded {
    pub use crate::demo::migrate::DemoMig;
    pub use crate::demo::rooms::{ShardedRoomExt, ShardedSpatialRoomExt};
    pub use crate::demo::wire::StripPos;
    pub use gsb_kit::sharded::{KitMig, ShardParkRecord};
    pub use gsb_kit::space::{grid_shape, shard_at};

    /// One shard of the demo's sharded world: the kit's generic
    /// [`ShardedRoom`](gsb_kit::sharded::ShardedRoom) instantiated with
    /// the demo's `Game` over the kit's 2D grid partition (constructors:
    /// [`ShardedRoomExt`]'s `new`, `with_economy`; the kit's
    /// `with_disconnect_grace`).
    pub type ShardedRoom = gsb_kit::sharded::ShardedRoom<
        crate::demo::play::DemoGame,
        gsb_kit::space::GridPartition2<crate::demo::components::Position>,
    >;

    /// One shard of the demo's sharded × spatial world: the kit's generic
    /// [`ShardedSpatialRoom`](gsb_kit::sharded::ShardedSpatialRoom)
    /// over the demo shard and the kit's 2D grid AOI (constructors:
    /// [`ShardedSpatialRoomExt`]'s `new`, `with_economy`; the kit's
    /// `with_disconnect_grace`).
    pub type ShardedSpatialRoom = gsb_kit::sharded::ShardedSpatialRoom<
        crate::demo::play::DemoGame,
        gsb_kit::space::GridPartition2<crate::demo::components::Position>,
        gsb_kit::space::Grid2,
    >;

    /// The demo's migrating entity state: the demo's captured components
    /// in the kit's envelope (`KitMig` — the park record rides along).
    pub type ShardedRoomState = KitMig<DemoMig>;
}

/// The open-visibility room and the demo's spawn distribution
/// (compatibility path).
pub mod room {
    pub use crate::demo::rooms::OpenRoomExt;
    pub use crate::demo::spawn::{DEFAULT_SPAWN_HALF, spawn_pos};

    /// The open-visibility room running the demo game: the kit's generic
    /// [`OpenRoom`](gsb_kit::room::OpenRoom) instantiated with the
    /// demo's `Game` (constructors: [`OpenRoomExt`]'s `new`,
    /// `with_spawn_half`, `with_economy`; the kit's
    /// `with_disconnect_grace`).
    pub type OpenRoom = gsb_kit::room::OpenRoom<crate::demo::play::DemoGame>;
}

/// ECS components of the demo game (compatibility path: the demo's
/// components plus the kit-owned [`WireId`](components::WireId)).
pub mod components {
    pub use crate::demo::components::{DEFAULT_SPEED, MoveTarget, Position, Speed};
    pub use gsb_kit::identity::WireId;
}
