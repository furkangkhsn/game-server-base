//! The reusable half of the crate (KIT-ARCHITECTURE §3: the future
//! `gsb-kit`): the visibility strategies, the cell-delta engine, the
//! sharded composites, park/resume bookkeeping, the input seq/ack rule,
//! wire-identity minting and the snapshot/`Private` framing.
//!
//! - `room` (`all`: `OpenRoom`), `aoi` (`AoiRoom`), `team` (`TeamRoom`),
//!   `pvs` (`SectorRoom`), `sharded` (`ShardedRoom`,
//!   `ShardedSpatialRoom`) — the strategies, one module each;
//! - `common` — what every strategy shares (cell-delta engine, park
//!   policy, input state, `Private` framing, orphan stamping, minting);
//! - `identity` — the kit-owned wire identity (`WireId`, §4.4);
//! - `seam` — **temporary**: the one module through which kit code still
//!   reaches the demo game (the phase-1 work list, see its docs).
//!
//! The dependency points one way in the target design (§3: "kit, demo'yu
//! asla görmez"); until phase 1 inverts it, the rule enforced here is
//! the weaker, structural one: every kit→demo reference is an import
//! from `seam`, and nothing else under `kit/` names a crate path outside
//! `crate::kit::`.

pub mod aoi;
pub(crate) mod common;
pub mod identity;
pub mod pvs;
pub mod room;
pub(crate) mod seam;
pub mod sharded;
pub mod team;

/// The demo rooms' default disconnect-park grace (RECONNECT §3): how
/// long a dropped transport's hero stays parked before its hold ends
/// toward the bot handover. The single source of the default — referenced
/// by `config.example.toml`'s `disconnect_grace_secs` documentation and
/// by the server config's `Default` impl, so they cannot drift.
pub const DEFAULT_DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
