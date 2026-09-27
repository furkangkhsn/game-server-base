//! A shard's team export (`docs/CROSS-SHARD.md` §8b.2): handed to the
//! room's hub, and the relays the hub could not queue counted (B72). A
//! CHILD module, so the tables and counters stay private.

use std::fmt::Debug;
use std::hash::Hash;

use crate::id::RoomId;
use crate::registry::actor::Registry;
use crate::registry::*;
use crate::shard::TeamExport;

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Relay one shard's export through the hub of THIS incarnation of
    /// the room (another incarnation's, or an unknown room's, export is
    /// a no-op). Table-only and synchronous: the relays are `try_send`s.
    ///
    /// A relay a target refused is counted by cause into the registry's
    /// cumulative totals, and a sample goes out at once (the registry
    /// otherwise samples only when a table changes, and a relay changes
    /// none): a FULL target (not keeping up — the source's next export
    /// carries the whole set again) or a CLOSED one (stopped or dead).
    pub(super) fn on_team_export(
        &mut self,
        room: RoomId,
        generation: u64,
        from: usize,
        tick: u64,
        export: TeamExport,
    ) {
        let Some(group) = self
            .rooms
            .get_mut(&room)
            .filter(|e| e.generation == generation)
            .and_then(|e| e.shards.as_mut())
        else {
            return;
        };
        let ShardGroup {
            teams, mailboxes, ..
        } = group;
        let drops = teams.on_export(room, from, tick, export, mailboxes);
        if drops == RelayDrops::default() {
            return;
        }
        self.reg_team_relays_dropped_full += drops.full;
        self.reg_team_relays_dropped_closed += drops.closed;
        self.emit_metrics();
    }
}

#[cfg(test)]
mod tests;
