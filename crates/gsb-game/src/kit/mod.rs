//! The reusable half of the crate (KIT-ARCHITECTURE §3: the future
//! `gsb-kit`): the visibility strategies, the cell-delta engine, the
//! sharded composites, park/resume bookkeeping, the input seq/ack rule,
//! wire-identity minting and the snapshot/`Private` framing.

pub mod identity;

/// The demo rooms' default disconnect-park grace (RECONNECT §3): how
/// long a dropped transport's hero stays parked before its hold ends
/// toward the bot handover. The single source of the default — referenced
/// by `config.example.toml`'s `disconnect_grace_secs` documentation and
/// by the server config's `Default` impl, so they cannot drift.
pub const DEFAULT_DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
