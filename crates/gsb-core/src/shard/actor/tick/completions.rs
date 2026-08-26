//! Phase 0b — deferred shard-RPC completions and the timeout sweep.

use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;


use crate::id::ConnectionId;
use crate::rpc::{PendingRequest, RpcReply, TIMEOUT_REASON};

use crate::shard::actor::ShardActor;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip rides every exchange and view; the bounds mirror what
    // the delta protocol does with it (diff via PartialEq, clone into
    // each neighbor's message, store in the actor's maps).
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Tick phase 0b — deferred shard-RPC completions (Faz 3), plus the
    /// pending-request timeout sweep that owns the client-visible answer.
    pub(crate) fn phase_completions(&mut self) {
    // -- Phase 0b — shard-RPC deferred completions (the Faz 3
    //    promotion; see `crate::rpc`): drain the worker reports
    //    (non-blocking — this shard never awaits a worker; the same
    //    try_recv discipline as the control drain above) and reconcile
    //    each report against the pending set. Exactly one answer per
    //    request is structural: a report for an id that is no longer
    //    pending (already answered, timed out below, its session left,
    //    OR its session migrated to another shard) is dropped and
    //    counted (`requests_late`).
    while let Ok(rep) = self.completions.try_recv() {
        let Some(deq) = self.pending.get_mut(&rep.conn) else {
            self.m.requests_late += 1;
            continue;
        };
        match deq.iter().position(|p| p.id == rep.id) {
            Some(idx) => {
                // The inner op comes from the pending entry (the
                // worker's report is outcome-only — this actor is the
                // authority on the request's shape).
                let op = deq[idx].op;
                deq.remove(idx);
                self.pending_total -= 1;
                if deq.is_empty() {
                    self.pending.remove(&rep.conn);
                }
                self.queued.entry(rep.conn).or_default().push(RpcReply {
                    id: rep.id,
                    ok: rep.ok,
                    op,
                    reason: rep.reason,
                    payload: rep.payload,
                });
            }
            None => self.m.requests_late += 1,
        }
    }
    // Sweep expired pending requests (the client-visible timeout):
    // per connection the deadlines are non-decreasing (same timeout,
    // FIFO arrivals), so only the head of each deque can be due. The
    // tick body stays synchronous: a wall-clock comparison, no await.
    // Cost when quiet: one `is_empty` probe (a shard with no pending
    // requests pays nothing below it).
    if !self.pending.is_empty() {
        let now = Instant::now();
        let mut due: Vec<(ConnectionId, PendingRequest)> = Vec::new();
        for (conn, deq) in self.pending.iter_mut() {
            if let Some(front) = deq.front()
                && front.due <= now
                && let Some(p) = deq.pop_front()
            {
                self.pending_total -= 1;
                due.push((*conn, p));
            }
        }
        self.pending.retain(|_, deq| !deq.is_empty());
        for (conn, p) in due {
            self.m.requests_timed_out += 1;
            self.queued.entry(conn).or_default().push(RpcReply {
                id: p.id,
                ok: false,
                op: p.op,
                reason: TIMEOUT_REASON.to_string(),
                payload: bytes::Bytes::new(),
            });
        }
    }

    }
}
