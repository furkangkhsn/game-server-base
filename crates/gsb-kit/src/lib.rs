//! gsb-kit — pluggable game components for gsb (`docs/KIT-ARCHITECTURE.md`).
//!
//! The kit decides **who sees what, and when**; the game decides what
//! the bytes are and how it plays (§2). Every room here is a complete
//! [`GameLogic`](gsb_core::room::GameLogic) generic over the game: a game
//! implements the seams, picks a room and a preset, and gets the
//! visibility strategy, the cell-delta engine, the sharded composites,
//! park/resume, the input sequence rule, wire identity and the snapshot /
//! `Private` envelopes without writing any of it.
//!
//! - [`codec`] (`RecordCodec`, §4.1), [`space`] (`Planar`, `CellSpace`,
//!   `Vision`, `SectorMap`, `Partition` and their presets `Grid2`,
//!   `VisionGrid2`, `ConvexSectors2`, `GridPartition2`, §4.2/§7),
//!   [`game`] (`Game` and its strategy extensions `TeamGame`,
//!   `ShardGame`, §4.3) — the seams a game implements;
//! - [`room`] (`OpenRoom<G>`), [`aoi`] (`AoiRoom<G, S>`), [`team`]
//!   (`TeamRoom<G, V>`), [`pvs`] (`SectorRoom<G, M>`), [`sharded`]
//!   (`ShardedRoom<G, P>`, `ShardedSpatialRoom<G, P, S>`) — the
//!   strategies, one module each;
//! - `common` (private) — what every strategy shares: the cell-delta
//!   engine and the snapshot envelope, park policy, the input sequence
//!   rule, `Private` framing, orphan stamping, the room accounting around
//!   the `Game` hooks;
//! - [`identity`] — the kit-owned wire identity (`WireId`) and its single
//!   minter (§4.4);
//! - [`proto`] — the kit's own envelope messages (§5).
//!
//! **The dependency points one way** (§3: "kit, demo'yu asla görmez"):
//! the kit depends on the engine crates only, never on a game — not even
//! in its tests, which run a small fixture game of their own
//! (`testing`, compiled for tests only). The rule is structural: a game
//! crate depends on the kit, so the kit depending back on it is a cargo
//! dependency cycle; the `manifest` test also keeps the kit's
//! `[dev-dependencies]` free of games.

pub mod aoi;
pub mod codec;
mod common;
pub mod game;
pub mod identity;
pub mod pvs;
pub mod room;
pub mod sharded;
pub mod space;
pub mod team;

#[cfg(test)]
mod manifest;
#[cfg(test)]
mod testing;

/// The kit's envelope messages (package `gsb.kit`, file `kit.proto`):
/// `WorldSnapshot`, `Private`, `InputAck`. Entity records and cell exits
/// are opaque game bytes here; a game's own proto declares the typed
/// mirror its clients decode with (KIT-ARCHITECTURE §5).
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/gsb.kit.rs"));

    #[cfg(test)]
    mod tests;
}

/// The kit rooms' default disconnect-park grace (RECONNECT §3): how
/// long a dropped transport's hero stays parked before its hold ends
/// toward the bot handover. The single source of the default — referenced
/// by `config.example.toml`'s `disconnect_grace_secs` documentation and
/// by the server config's `Default` impl, so they cannot drift.
pub const DEFAULT_DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
