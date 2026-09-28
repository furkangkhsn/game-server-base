//! What a stopping room (or shard) still holds, and its last word to the
//! collector (BACKLOG B62).
//!
//! A room stops on a `Shutdown` (its destroy) or when the ticker closes
//! (the server stops). Until B62 it then sent nothing more: what its
//! sessions still had — input unread in their action channels, answers
//! owed and not yet shipped, requests in flight at a worker — was never
//! counted, and neither was anything counted since its last periodic
//! sample (up to one report period). Now the stop ends every session the
//! way a leave does, as far as the COUNTING goes (no logic hook runs: the
//! logic has already had `on_shutdown` and its match result), and the
//! room hands the collector one final sample.
//!
//! The final sample is its own event, [`MetricsEvent::RoomFinal`], so the
//! collector can take it where it refuses a periodic straggler (a room
//! inside its destroyed-room linger), and so it can start that linger
//! itself (the room is gone — see the accumulator). It is delivered with
//! the stop-message idiom ([`crate::channel::post`]): in place, or from a
//! spawned sender when the metrics channel is full — never dropped for a
//! full channel, and never parking the stopping actor. Only a collector
//! that is already gone loses it — and at the server's stop it is not:
//! its final report waits until every room (every sender) has ended
//! (BACKLOG F35), so only a room still running at the collector's grace
//! (`crate::metrics::FINAL_REPORT_GRACE`) misses it.

use std::collections::{HashMap, VecDeque};

use crate::channel::{Mailbox, post};
use crate::id::{ConnectionId, PlayerId};
use crate::metrics::{MetricsEvent, RoomSample, VerdictsLost};
use crate::registry::{CloseRequest, LeaveRequest};
use crate::room::{RoomConn, RoomCounters, drop_unread};
use crate::rpc::RpcReply;

/// The session state a stopping actor still holds.
pub(crate) struct Held<'a, G, P> {
    pub(crate) conns: &'a mut HashMap<PlayerId, RoomConn<G>>,
    pub(crate) pending: &'a mut HashMap<ConnectionId, VecDeque<P>>,
    pub(crate) pending_total: &'a mut usize,
    pub(crate) queued: &'a mut HashMap<ConnectionId, Vec<RpcReply>>,
}

impl RoomCounters {
    /// Count what the stop takes along, with the counters a session end
    /// uses: every row's unread input (`requests_dropped_unread` /
    /// `actions_dropped_unread` — parked rows included, their dead
    /// channels were never pulled), the requests still in flight
    /// (`requests_abandoned`) and the answers still owed
    /// (`requests_undelivered`). The rows stay (the actor is about to
    /// drop them); the request state is emptied, so the final sample's
    /// `pending_requests` gauge reads 0.
    pub(crate) fn count_at_stop<G, P>(&mut self, held: Held<'_, G, P>) {
        for rc in held.conns.values_mut() {
            self.count_unread(drop_unread(&mut rc.actions));
        }
        for (_, deq) in held.pending.drain() {
            self.requests_abandoned += deq.len() as u64;
        }
        *held.pending_total = 0;
        for (_, owed) in held.queued.drain() {
            self.requests_undelivered += owed.len() as u64;
        }
    }
}

/// Hand the collector a stopping actor's final sample (see the module
/// docs): never dropped for a full channel, never awaited.
pub(crate) fn send_final(metrics: &Mailbox<MetricsEvent>, sample: RoomSample) {
    post(metrics, MetricsEvent::RoomFinal(sample));
}

impl RoomCounters {
    /// The session verdicts a stopping room or shard still holds for the
    /// registry (BACKLOG F56): the close and leave requests and the
    /// detach-despawn reports its flushes kept for a full mailbox. The
    /// stop ends the retrying, so each is a verdict never carried out —
    /// counted by kind, the close by its reason, with the ones the
    /// registry's closed mailbox refused (`crate::registry::close`).
    pub(crate) fn count_unsent_verdicts(
        &mut self,
        closes: &mut Vec<CloseRequest>,
        leaves: &mut Vec<LeaveRequest>,
        despawns: &mut Vec<ConnectionId>,
    ) {
        for req in closes.drain(..) {
            self.verdicts_lost.close(req.cause);
        }
        self.verdicts_lost.leaves += leaves.drain(..).count() as u64;
        self.verdicts_lost.detach_despawns += despawns.drain(..).count() as u64;
    }
}

/// Hand the collector the session verdicts a stop found lost (F56; a
/// room's, a shard's or the registry's), stop-message idiom like the
/// final sample — nothing at all when there are none.
pub(crate) fn send_verdicts_lost(metrics: &Mailbox<MetricsEvent>, lost: &VerdictsLost) {
    if !lost.is_empty() {
        post(metrics, MetricsEvent::VerdictsLost(*lost));
    }
}
