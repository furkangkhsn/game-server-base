//! The room's stop: the logic's last hooks, the match result, and the
//! final count (BACKLOG B62 — `crate::room::stop`).

use std::fmt::Debug;
use std::hash::Hash;

use tracing::{debug, warn};

use crate::room::actor::RoomActor;
use crate::room::{Held, send_final};

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Everything after the tick loop: `on_shutdown`, the match result,
    /// then what the room still holds, counted, and the final sample.
    /// Synchronous — no await is added to the stop.
    pub(in crate::room) fn finish(&mut self) {
        self.logic.on_shutdown();
        // The match-result seam (control plane): the logic computes the
        // final result from the world (still alive — it is dropped only
        // when `self` drops) and the room reports it to the sink with
        // the synchronous `try_send` (no await: the room's only await
        // stayed `tick_rx.recv()`). Best effort — a full or gone sink
        // drops the result, warns and tells the collector (B57; a slow
        // result consumer must not stall the room's teardown).
        if let Some(result) = self.logic.match_result(&mut self.world)
            && let Some(sink) = &self.result_sink
        {
            let result = crate::registry::MatchResult {
                room: self.config.id,
                payload: result,
            };
            match crate::registry::send_match_result(sink, result, &self.metrics) {
                Ok(()) => debug!(room = %self.config.id, "match result reported"),
                Err(crate::metrics::MatchResultDrop::Full) => {
                    warn!(room = %self.config.id, "match result dropped: sink full");
                }
                Err(crate::metrics::MatchResultDrop::Closed) => {
                    debug!(room = %self.config.id, "match result dropped: sink gone");
                }
            }
        }
        // The stop ends every session: count what they take along, as a
        // session end does (B62), then hand the collector the final
        // sample — the counters since the last periodic one included.
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
