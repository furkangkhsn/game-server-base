//! The receiver side of the team exchange: one slot per source shard,
//! replaced wholesale, expired by silence, merged per team.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use bytes::Bytes;

use crate::shard::{TEAM_EXPORT_MAX_RECORDS, TEAM_EXPORT_TTL_TICKS, TeamImport, TeamRecord};

/// One imported record, as the logic reads it: "this wire is visible to
/// the team, and this is its record as its source encoded it".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedRecord {
    /// The entity's wire identity.
    pub wire: u64,
    /// The encoded record body (the game's codec output, verbatim).
    pub bytes: Bytes,
    /// The source shard of the winning copy.
    pub from: usize,
    /// The source's tick for the winning copy.
    pub tick: u64,
}

/// One source shard's latest import.
#[derive(Debug)]
struct Slot {
    tick: u64,
    records: Vec<TeamRecord>,
}

/// The other shards' team records as this shard holds them
/// (`docs/CROSS-SHARD.md` §8b.3), handed to
/// [`crate::shard::ShardLogic::team_exchange`] every tick.
///
/// **Bounded.** One slot per source shard; a slot holds at most
/// [`TEAM_EXPORT_MAX_RECORDS`] records (the tail of an oversized import
/// is cut and reported); a slot silent for [`TEAM_EXPORT_TTL_TICKS`]
/// ticks is dropped.
///
/// **Merge rule.** Per team, one record per wire: when several sources
/// name the same wire (a member crossing a seam is in the old shard's
/// export and the new one's for a tick), the NEWEST tick wins, a tie
/// goes to the lower source index. The per-team lists are sorted by
/// wire (deterministic).
///
/// The core owns the one instance of a shard; [`Self::insert`],
/// [`Self::expire`] and [`Self::settle`] are public so a logic's unit
/// tests can build the view the actor would hand them.
#[derive(Debug, Default)]
pub struct TeamImports {
    slots: Vec<Option<Slot>>,
    merged: BTreeMap<u64, Vec<ImportedRecord>>,
    dirty: bool,
}

impl TeamImports {
    /// Replace `import.from`'s slot with `import` (an empty import clears
    /// it). An import OLDER than the slot's current one is ignored (a
    /// source's imports arrive in order in process; the guard keeps a
    /// reordering link from resurrecting a stale set). Returns how many
    /// records were cut by the per-slot cap.
    pub fn insert(&mut self, import: TeamImport) -> usize {
        let TeamImport {
            from,
            tick,
            mut records,
        } = import;
        if self.slots.len() <= from {
            self.slots.resize_with(from + 1, || None);
        }
        if let Some(slot) = &self.slots[from]
            && slot.tick > tick
        {
            return 0;
        }
        let cut = records.len().saturating_sub(TEAM_EXPORT_MAX_RECORDS);
        records.truncate(TEAM_EXPORT_MAX_RECORDS);
        // An emptied slot stays (with its tick) so the order guard above
        // still holds for it; it holds no record and expires like any.
        self.slots[from] = Some(Slot { tick, records });
        self.dirty = true;
        cut
    }

    /// Drop every slot silent for [`TEAM_EXPORT_TTL_TICKS`] ticks at
    /// tick `now`. Returns how many of them still held records (the
    /// ghosts the TTL removed).
    pub fn expire(&mut self, now: u64) -> usize {
        let mut dropped = 0;
        for slot in &mut self.slots {
            if let Some(s) = slot
                && now.saturating_sub(s.tick) >= TEAM_EXPORT_TTL_TICKS
            {
                if !s.records.is_empty() {
                    dropped += 1;
                }
                *slot = None;
            }
        }
        if dropped > 0 {
            self.dirty = true;
        }
        dropped
    }

    /// Rebuild the per-team view after inserts/expiry (a no-op when
    /// nothing changed since the last call).
    pub fn settle(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.merged.clear();
        for (from, slot) in self.slots.iter().enumerate() {
            let Some(slot) = slot else { continue };
            for r in &slot.records {
                self.merged.entry(r.team).or_default().push(ImportedRecord {
                    wire: r.wire,
                    bytes: r.bytes.clone(),
                    from,
                    tick: slot.tick,
                });
            }
        }
        for list in self.merged.values_mut() {
            list.sort_unstable_by_key(|r| (r.wire, Reverse(r.tick), r.from));
            list.dedup_by_key(|r| r.wire);
        }
    }

    /// The merged records visible to `team` (sorted by wire; empty when
    /// no other shard exported any). Reflects the last [`Self::settle`].
    pub fn team(&self, team: u64) -> &[ImportedRecord] {
        self.merged.get(&team).map_or(&[], Vec::as_slice)
    }

    /// The teams with at least one imported record, ascending.
    pub fn teams(&self) -> impl Iterator<Item = u64> + '_ {
        self.merged.keys().copied()
    }

    /// Merged records over every team.
    pub fn len(&self) -> usize {
        self.merged.values().map(Vec::len).sum()
    }

    /// No imported record at all.
    pub fn is_empty(&self) -> bool {
        self.merged.is_empty()
    }

    /// Source shards whose slot currently holds records.
    pub fn sources(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.as_ref().is_some_and(|s| !s.records.is_empty()))
            .count()
    }
}
