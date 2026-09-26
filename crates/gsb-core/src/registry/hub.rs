//! The team-export hub (`docs/CROSS-SHARD.md` §8b.2): the registry's
//! relay between the shards of one sharded room. Byte-encoded records in,
//! byte-encoded records out — the hub never decodes one, and it keeps no
//! record: each export is relayed as it arrives, filtered per target to
//! the teams that target views.

use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, info};

use crate::channel::Mailbox;
use crate::id::RoomId;
use crate::shard::{
    ShardMsg, TEAM_EXPORT_TTL_TICKS, TEAM_HUB_SWEEP_EVERY_TICKS, TeamExport, TeamImport, TeamRecord,
};

/// How often (in export ticks) the hub logs its `team_hub_summary` line
/// for a room — ~8.5 s at 30 Hz.
const TEAM_HUB_SUMMARY_EVERY_TICKS: u64 = 256;

/// One source shard's subscription state.
#[derive(Debug, Clone)]
struct HubSlot {
    /// The tick of the source's latest export (the TTL clock).
    tick: u64,
    /// The teams the source hosts viewers of, sorted, deduplicated.
    views: Vec<u64>,
    /// Per target shard: the target holds a NON-EMPTY import from this
    /// source. An export with nothing for such a target still goes to it
    /// — once, empty — so its slot clears now rather than at the TTL.
    relayed: Vec<bool>,
}

/// Per-window hub counters (the `team_hub_summary` line).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HubStats {
    /// Exports received (of this room's current incarnation).
    pub(crate) exports: u64,
    /// Imports queued on a target shard's mailbox.
    pub(crate) relays: u64,
    /// Imports a full or closed target mailbox refused (the source's
    /// next export carries the whole set again).
    pub(crate) relay_drops: u64,
    /// Records in the queued imports.
    pub(crate) relay_records: u64,
    /// Source slots dropped by the TTL sweep.
    pub(crate) expired: u64,
}

/// One sharded room's hub, kept inside the registry's room entry
/// (`ShardGroup::teams`): destroyed with it, so another room — or another
/// incarnation of this one — never sees its state.
#[derive(Debug, Clone, Default)]
pub(crate) struct TeamHub {
    /// Per source shard: its latest subscription (`None` = never exported
    /// or expired).
    slots: Vec<Option<HubSlot>>,
    /// The tick of the last TTL sweep.
    last_sweep: Option<u64>,
    /// The tick of the last summary line.
    last_summary: Option<u64>,
    /// Counters of the current summary window.
    pub(crate) stats: HubStats,
}

impl TeamHub {
    /// Take source `from`'s export of `tick` and relay it: to every OTHER
    /// shard that has exported (and not expired), the records of the
    /// teams that shard views — `try_send`, never awaited. Isolation is
    /// this filter: a team's records reach only shards that list the team
    /// among their viewed teams.
    pub(crate) fn on_export<St, Sp>(
        &mut self,
        room: RoomId,
        from: usize,
        tick: u64,
        export: TeamExport,
        mailboxes: &[Mailbox<ShardMsg<St, Sp>>],
    ) {
        let n = mailboxes.len();
        if from >= n {
            debug!(room = %room, from, "team export from an unknown shard index; ignored");
            return;
        }
        if self.slots.len() < n {
            self.slots.resize_with(n, || None);
        }
        // The source's own slot is replaced below, not swept.
        self.sweep(tick, from);
        self.stats.exports += 1;
        let TeamExport {
            mut views, records, ..
        } = export;
        views.sort_unstable();
        views.dedup();
        let relayed = match self.slots[from].take() {
            Some(prev) => prev.relayed,
            None => vec![false; n],
        };
        let mut slot = HubSlot {
            tick,
            views,
            relayed,
        };
        // The source's own slot is out of the table (taken above) while
        // it relays: it never relays to itself.
        for (t, mailbox) in mailboxes.iter().enumerate() {
            let Some(target) = self.slots[t].as_ref() else {
                continue;
            };
            let picked: Vec<TeamRecord> = records
                .iter()
                .filter(|r| target.views.binary_search(&r.team).is_ok())
                .cloned()
                .collect();
            if picked.is_empty() && !slot.relayed[t] {
                continue;
            }
            let count = picked.len();
            let import = TeamImport {
                from,
                tick,
                records: picked,
            };
            match mailbox.try_send(ShardMsg::TeamImport(import)) {
                Ok(()) => {
                    slot.relayed[t] = count > 0;
                    self.stats.relays += 1;
                    self.stats.relay_records += count as u64;
                }
                Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => {
                    self.stats.relay_drops += 1;
                }
            }
        }
        self.slots[from] = Some(slot);
        self.summary(room, tick);
    }

    /// Drop the subscriptions of sources silent for the TTL — at most
    /// once per [`TEAM_HUB_SWEEP_EVERY_TICKS`] (an O(shards) retain,
    /// amortized over the exports in between). `exporting` is the shard
    /// whose export triggered the sweep: alive by definition.
    fn sweep(&mut self, now: u64, exporting: usize) {
        if self
            .last_sweep
            .is_some_and(|at| now.saturating_sub(at) < TEAM_HUB_SWEEP_EVERY_TICKS)
        {
            return;
        }
        self.last_sweep = Some(now);
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if i != exporting
                && slot
                    .as_ref()
                    .is_some_and(|s| now.saturating_sub(s.tick) >= TEAM_EXPORT_TTL_TICKS)
            {
                *slot = None;
                self.stats.expired += 1;
            }
        }
    }

    /// One `team_hub_summary` line per room per
    /// [`TEAM_HUB_SUMMARY_EVERY_TICKS`] (window counters, then reset).
    fn summary(&mut self, room: RoomId, tick: u64) {
        // The first export opens the window; a line closes a full one.
        let at = *self.last_summary.get_or_insert(tick);
        if tick.saturating_sub(at) < TEAM_HUB_SUMMARY_EVERY_TICKS {
            return;
        }
        self.last_summary = Some(tick);
        let s = std::mem::take(&mut self.stats);
        info!(
            room = %room,
            exports = s.exports,
            relays = s.relays,
            relay_drops = s.relay_drops,
            relay_records = s.relay_records,
            expired = s.expired,
            "team_hub_summary"
        );
    }

    /// The teams source `from` currently views (`None` = no live
    /// subscription) — for tests.
    #[cfg(test)]
    pub(crate) fn views(&self, from: usize) -> Option<&[u64]> {
        self.slots.get(from)?.as_ref().map(|s| s.views.as_slice())
    }
}

#[cfg(test)]
mod tests;
