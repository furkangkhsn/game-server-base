//! The reusable half of the crate (KIT-ARCHITECTURE §3: the future
//! `gsb-kit`): the visibility strategies, the cell-delta engine, the
//! sharded composites, park/resume bookkeeping, the input seq/ack rule,
//! wire-identity minting and the snapshot/`Private` framing.
//!
//! - `codec` (`RecordCodec`, §4.1), `space` (`Planar`, `CellSpace`,
//!   `Vision`, `SectorMap`, `Partition` and their 2D presets `Grid2`,
//!   `VisionGrid2`, `ConvexSectors2`, `GridPartition2`, §4.2/§7), `game`
//!   (`Game` and its strategy extensions `TeamGame`, `ShardGame`, §4.3)
//!   — the seams a game implements;
//! - `room` (`OpenRoom<G>`), `aoi` (`AoiRoom<G, S>`), `team`
//!   (`TeamRoom<G, V>`), `pvs` (`SectorRoom<G, M>`), `sharded`
//!   (`ShardedRoom<G, P>`, `ShardedSpatialRoom<G, P, S>`) — the
//!   strategies, one module each, every one generic over the game since
//!   phase 1;
//! - `common` — what every strategy shares (cell-delta engine and the
//!   snapshot envelope, park policy, the input sequence rule, `Private`
//!   framing, orphan stamping, the room accounting around the `Game`
//!   hooks);
//! - `identity` — the kit-owned wire identity (`WireId`) and its single
//!   `Minter` (§4.4);
//! - `testing` (tests only) — the kit's own fixture game and test
//!   wrappers.
//!
//! The dependency points one way (§3: "kit, demo'yu asla görmez"): the
//! kit's envelope comes from its own proto (`gsb_kit::proto`), its tests
//! run the fixture game, and nothing under `kit/` names a crate path
//! outside `crate::kit` (the `layering` test). The crate split (phase 2)
//! moves this module into `gsb-kit`.

pub mod aoi;
pub mod codec;
pub(crate) mod common;
pub mod game;
pub mod identity;
pub mod pvs;
pub mod room;
pub mod sharded;
pub mod space;
pub mod team;
#[cfg(test)]
mod testing;

/// The demo rooms' default disconnect-park grace (RECONNECT §3): how
/// long a dropped transport's hero stays parked before its hold ends
/// toward the bot handover. The single source of the default — referenced
/// by `config.example.toml`'s `disconnect_grace_secs` documentation and
/// by the server config's `Default` impl, so they cannot drift.
pub const DEFAULT_DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
