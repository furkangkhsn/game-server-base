//! The team exchange (`docs/CROSS-SHARD.md` §8, §8b): what a shard tells
//! the registry hub about each team's VISIBLE SET, and what it keeps of
//! the other shards' sets.
//!
//! Anti-local interest: a team's members are scattered over shards, and
//! the border strip covers only the seams. So every shard exports, per
//! team, the records that team sees HERE (its members plus the enemies
//! its units here see) — byte-encoded by the game's codec, so neither
//! the registry nor this crate ever decodes one — and the registry
//! relays each export to the OTHER shards that host viewers of those
//! teams. A receiving shard keeps one slot per source shard, replaced
//! wholesale by every import, expired after [`TEAM_EXPORT_TTL_TICKS`]
//! of silence, and merged per team for the logic
//! ([`crate::shard::ShardLogic::team_exchange`]).

use bytes::Bytes;

mod imports;

pub use imports::{ImportedRecord, TeamImports};

/// A source that has not exported for this many ticks is dropped — its
/// import slots on every receiver and its subscriptions in the hub. A
/// live source refreshes every tick, so the TTL bounds only how long a
/// SILENT source lingers: a dead shard, or one whose last (clearing)
/// export was lost after its last member left. ~2 s at 30 Hz.
pub const TEAM_EXPORT_TTL_TICKS: u64 = 64;

/// The hub sweeps expired subscriptions of a room at most once per this
/// many ticks (by the exports' ticks: the registry has no clock).
pub const TEAM_HUB_SWEEP_EVERY_TICKS: u64 = 64;

/// The hard cap on records in ONE export (summed over its teams),
/// whatever the game's budget: what bounds the registry's relays and
/// every receiver's slots. Over the cap the tail is cut and counted.
pub const TEAM_EXPORT_MAX_RECORDS: usize = 16_384;

/// The hard cap on the viewed teams one export may list.
pub const TEAM_EXPORT_MAX_VIEWS: usize = 256;

/// One record of a team's visible set: the team whose viewers get it,
/// the entity's wire identity, and its record body exactly as the
/// game's codec encodes it (opaque here and in the registry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRecord {
    /// The team whose viewers see this record.
    pub team: u64,
    /// The entity's wire identity (core vocabulary: unique in the room).
    pub wire: u64,
    /// The encoded record body (the game's codec output).
    pub bytes: Bytes,
}

/// What a shard logic hands the core for one tick
/// ([`crate::shard::ShardLogic::team_exchange`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeamExport {
    /// The teams this shard hosts VIEWERS of (players' teams): the hub
    /// relays other shards' records of these teams here — and only
    /// these.
    pub views: Vec<u64>,
    /// This shard's visible set per team (members first is the kit's
    /// convention; the core keeps the order and cuts the tail over
    /// [`TEAM_EXPORT_MAX_RECORDS`]).
    pub records: Vec<TeamRecord>,
}

impl TeamExport {
    /// Nothing viewed, nothing seen.
    pub fn is_empty(&self) -> bool {
        self.views.is_empty() && self.records.is_empty()
    }
}

/// What the hub relays to one shard: the source's records of the teams
/// this shard views, from the source's export at `tick`. It REPLACES the
/// receiver's slot for `from` (an empty one clears it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamImport {
    /// The source shard.
    pub from: usize,
    /// The source's tick when it exported (TTL and dedup).
    pub tick: u64,
    /// The source's records of the teams this shard views.
    pub records: Vec<TeamRecord>,
}

/// Team-exchange counters of ONE shard actor, cumulative: the metrics
/// sample carries them as they are (`RoomSample::team_*`); the
/// `team_exchange_summary` line (the §7 border summary's shape) logs
/// their ~1 s window ([`Self::since`] the last logged snapshot), only
/// when something moved.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TeamStats {
    /// Exports queued on the registry's mailbox.
    pub(crate) exports: u64,
    /// Exports a full or closed registry mailbox refused (the next tick's
    /// export carries the same set again).
    pub(crate) export_drops: u64,
    /// Records in the queued exports.
    pub(crate) export_records: u64,
    /// Records (and viewed teams) cut by the core's hard caps.
    pub(crate) over_cap: u64,
    /// Imports applied (slots replaced).
    pub(crate) imports: u64,
    /// Records in the applied imports.
    pub(crate) import_records: u64,
    /// Source slots dropped by the TTL.
    pub(crate) expired: u64,
}

impl TeamStats {
    /// Whether the counters show any team traffic.
    pub(crate) fn any(&self) -> bool {
        *self != Self::default()
    }

    /// The traffic counted since `earlier` (a snapshot of these same
    /// cumulative counters): the log line's window.
    pub(crate) fn since(&self, earlier: &Self) -> Self {
        Self {
            exports: self.exports - earlier.exports,
            export_drops: self.export_drops - earlier.export_drops,
            export_records: self.export_records - earlier.export_records,
            over_cap: self.over_cap - earlier.over_cap,
            imports: self.imports - earlier.imports,
            import_records: self.import_records - earlier.import_records,
            expired: self.expired - earlier.expired,
        }
    }
}

#[cfg(test)]
mod tests;
