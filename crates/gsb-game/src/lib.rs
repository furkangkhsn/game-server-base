//! Demo game logic for gsb.
//!
//! This crate is the *only* place that knows anything about a particular
//! game. It plugs into the core via [`gsb_core::room::RoomLogic`] (see
//! [`room::DemoRoom`]) and registers its wire messages with the
//! [`gsb_protocol::MessageTable`] (see [`register`]).
//!
//! Everything here is intentionally small and replaceable: swap the
//! components, systems, and `RoomLogic` implementation for a real MOBA or
//! MMORPG without touching the core, net, protocol, or ecs crates.
//!
//! The demo game has four interchangeable `RoomLogic` rooms — the same
//! components, the same movement system, the same wire format, and only
//! the *visibility strategy* differs (selected in `gsb-server`'s config,
//! see `docs/DESIGN.md` §8):
//!
//! - [`room::DemoRoom`] — `GroupKey = ()`: everyone sees the whole world
//!   (the baseline, no grouping).
//! - [`aoi::AoiRoom`] — `GroupKey = Cell`: spatial AOI (3×3 cell block).
//! - [`team::TeamRoom`] — `GroupKey = Team`: team fog of war (2 groups;
//!   `group_of` looks at game state, not position).
//! - [`pvs::SectorRoom`] — `GroupKey = Sector`: per-map-segment PVS
//!   (static visibility table over hand-authored convex sectors).
//!
//! The fifth strategy is a different *topology*, not just a group key:
//! [`sharded::ShardedRoom`] partitions the world into a grid of shards
//! (each shard its own actor with its own `World`), plugged in via
//! [`gsb_core::shard::ShardLogic`] rather than `RoomLogic` — see
//! `docs/DESIGN.md` for the sharding design and the wire-id range
//! partitioning.
//!
//! Everything the rooms share that is *not* a visibility-strategy
//! decision (identity minting, the connection table, input ingestion, the
//! system run, orphan stamping) lives in [`common`], once.

/// The demo rooms' default disconnect-park grace (RECONNECT §3): how
/// long a dropped transport's hero stays parked before its hold ends
/// toward the bot handover. The single source of the default — referenced
/// by `config.example.toml`'s `disconnect_grace_secs` documentation and
/// by the server config's `Default` impl, so they cannot drift.
pub const DEFAULT_DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

pub mod aoi;
pub mod components;
pub mod economy;
pub mod op;
pub mod pvs;
pub mod room;
pub mod sharded;
pub mod systems;
pub mod team;

mod common;

/// Generated game protocol messages (package `gsb.game`, file `game.proto`).
pub mod game {
    include!(concat!(env!("OUT_DIR"), "/gsb.game.rs"));
}

use gsb_protocol::MessageTable;

/// Register the demo game's wire messages with `table`.
///
/// The server builds one [`MessageTable`] at startup (base messages + game
/// messages) and shares it read-only between all actors.
pub fn register(table: &mut MessageTable) {
    table.reg::<game::MoveTo>(op::MOVE_TO);
    table.reg::<game::WorldSnapshot>(op::WORLD_SNAPSHOT);
    table.reg::<game::Private>(op::PRIVATE);
}
