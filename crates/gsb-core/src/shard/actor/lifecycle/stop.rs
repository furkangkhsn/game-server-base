//! The shard's stop: the logic's last hooks, the match result, and the
//! final count (BACKLOG B62 — `crate::room::stop`; the room actor's
//! stop, per shard).

use std::fmt::Debug;
use std::hash::Hash;

use tracing::{debug, warn};

use crate::room::{Held, send_final, send_verdicts_lost};
use crate::shard::actor::ShardActor;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Everything after the tick loop: `on_shutdown`, this shard's match
    /// result, then what the shard still holds, counted, and its final
    /// sample (under its own sample id). Synchronous.
    pub(crate) fn finish(&mut self) {
        self.logic.on_shutdown();
        // The match-result seam (the Faz 3 promotion; see the module docs,
        // "Shard-RPC and match-result"): THIS shard reports ITS final state
        // through the shared sink under the LOGICAL room id. One logical
        // room therefore yields one payload PER SHARD (the platform's
        // adapter concatenates/filters; nothing was added to
        // `MatchResult`). Same best-effort discipline as the room actor:
        // a full or gone sink drops the result, warns/debugs and tells the
        // collector (B57) — a slow consumer must not stall the shard's
        // teardown, and the shard's only await stays `tick_rx.recv()`.
        if let Some(result) = self.logic.match_result(&mut self.world)
            && let Some(sink) = &self.result_sink
        {
            let result = crate::registry::MatchResult {
                room: self.config.id,
                payload: result,
            };
            match crate::registry::send_match_result(sink, result, &self.metrics) {
                Ok(()) => debug!(
                    room = %self.config.id,
                    shard = self.index,
                    "shard match result reported"
                ),
                Err(crate::metrics::MatchResultDrop::Full) => warn!(
                    room = %self.config.id,
                    shard = self.index,
                    "match result dropped: sink full"
                ),
                Err(crate::metrics::MatchResultDrop::Closed) => debug!(
                    room = %self.config.id,
                    shard = self.index,
                    "match result dropped: sink gone"
                ),
            }
        }
        // What the shard still holds beyond its sessions (B68): the
        // inbox's and the deferred queue's messages, the effects in
        // flight.
        self.count_leftovers();
        // The verdicts still queued for the registry (F56): lost with the
        // ones its closed mailbox refused, sent before the final sample.
        self.m.count_unsent_verdicts(
            &mut self.close_requests,
            &mut self.leave_requests,
            &mut self.despawn_reports,
        );
        send_verdicts_lost(&self.metrics, &self.m.verdicts_lost);
        // The stop ends every session this shard holds: count what they
        // take along, as a session end does (B62), then hand the
        // collector the final sample.
        self.m.count_at_stop(Held {
            conns: &mut self.conns,
            pending: &mut self.pending,
            pending_total: &mut self.pending_total,
            queued: &mut self.queued,
        });
        let sample = self.sample();
        send_final(&self.metrics, sample);
    }
}

/// The leftovers' count (B68). A CHILD module, so it reaches the actor's
/// state directly.
mod leftovers;
