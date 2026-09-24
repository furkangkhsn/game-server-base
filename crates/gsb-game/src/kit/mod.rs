//! The reusable half of the crate (KIT-ARCHITECTURE §3: the future
//! `gsb-kit`): the visibility strategies, the cell-delta engine, the
//! sharded composites, park/resume bookkeeping, the input seq/ack rule,
//! wire-identity minting and the snapshot/`Private` framing.
//!
//! - `codec` (`RecordCodec`, §4.1), `space` (`CellSpace` + the `Grid2`
//!   preset, §4.2), `game` (`Game`, §4.3) — the seams a game implements;
//! - `room` (`all`: `OpenRoom<G>`), `aoi` (`AoiRoom<G, S>`), `team`
//!   (`TeamRoom`), `pvs` (`SectorRoom`), `sharded` (`ShardedRoom`,
//!   `ShardedSpatialRoom`) — the strategies, one module each (the first
//!   two generic over the game since phase 1a);
//! - `common` — what every strategy shares (cell-delta engine and the
//!   snapshot envelope, park policy, the input sequence rule, `Private`
//!   framing, orphan stamping, the room accounting around the `Game`
//!   hooks);
//! - `identity` — the kit-owned wire identity (`WireId`) and its single
//!   `Minter` (§4.4);
//! - `seam` — **temporary**: the one module through which kit code still
//!   reaches the demo game (the phase-1b work list, see its docs).
//!
//! The dependency points one way in the target design (§3: "kit, demo'yu
//! asla görmez"); until phase 1 finishes inverting it, the rule enforced
//! here is the weaker, structural one: every kit→demo reference is an
//! import from `seam`, and nothing else under `kit/` names a crate path
//! outside `crate::kit`.

pub mod aoi;
pub mod codec;
pub(crate) mod common;
pub mod game;
pub mod identity;
pub mod pvs;
pub mod room;
pub(crate) mod seam;
pub mod sharded;
pub mod space;
pub mod team;

/// The demo rooms' default disconnect-park grace (RECONNECT §3): how
/// long a dropped transport's hero stays parked before its hold ends
/// toward the bot handover. The single source of the default — referenced
/// by `config.example.toml`'s `disconnect_grace_secs` documentation and
/// by the server config's `Default` impl, so they cannot drift.
pub const DEFAULT_DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
