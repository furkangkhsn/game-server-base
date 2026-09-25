//! Phase 5b — TEAMS (`docs/CROSS-SHARD.md` §8b): expire and merge the
//! other shards' team records, hand them to the logic with the borrowed
//! set, and send the logic's export to the registry hub.

use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::mpsc::error::TrySendError;
use tracing::debug;

use crate::registry::RegistryMsg;
use crate::room::TickCtx;

use crate::shard::actor::ShardActor;
use crate::shard::*;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Phase 5b. Synchronous: the send is a `try_send` on the registry's
    /// bounded mailbox, and a refused export is only counted — the next
    /// tick's export carries the whole set again (wholesale replacement
    /// makes every export self-healing).
    pub(crate) fn phase_teams(&mut self, ctx: &TickCtx, borrowed: &[BorderRecord<Sp>]) {
        // A source silent for the TTL is gone (a dead shard, a lost
        // clearing export): its records must not ghost here.
        self.tstats.expired += self.teams.expire(ctx.tick) as u64;
        self.teams.settle();
        let Some(mut export) =
            self.logic
                .team_exchange(&mut self.world, ctx, borrowed, &self.teams)
        else {
            return;
        };
        // The hard caps (§8b.1): whatever the game's budget, one export
        // never exceeds them — they bound the hub's relays and every
        // receiver's slot.
        let cut = export.records.len().saturating_sub(TEAM_EXPORT_MAX_RECORDS)
            + export.views.len().saturating_sub(TEAM_EXPORT_MAX_VIEWS);
        if cut > 0 {
            export.records.truncate(TEAM_EXPORT_MAX_RECORDS);
            export.views.truncate(TEAM_EXPORT_MAX_VIEWS);
            self.tstats.over_cap += cut as u64;
        }
        // Nothing now and nothing held elsewhere: no message. Nothing now
        // after something: ONE empty export clears the receivers.
        let listed = !export.is_empty();
        if !listed && !self.team_sent {
            return;
        }
        // A directly-driven shard (the test rigs) has no hub to tell.
        let Some(registry) = &self.registry else {
            return;
        };
        let records = export.records.len() as u64;
        match registry.try_send(RegistryMsg::TeamExport {
            room: self.config.id,
            // The install generation (the remote effects' epoch): the
            // hub drops the late export of a dead incarnation.
            generation: self.effects.out.epoch,
            from: self.index,
            tick: ctx.tick,
            export,
        }) {
            Ok(()) => {
                self.tstats.exports += 1;
                self.tstats.export_records += records;
                self.team_sent = listed;
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => {
                self.tstats.export_drops += 1;
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    "team export dropped: registry mailbox full or closed"
                );
            }
        }
    }
}
