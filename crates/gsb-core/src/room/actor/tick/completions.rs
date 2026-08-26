//! Phase 0b — deferred RPC completions.

use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;
use crate::id::ConnectionId;

use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Tick phase 0b — deferred RPC completions (see [`crate::rpc`]):
    /// drain the worker reports non-blockingly and reconcile them.
    pub(super) fn phase_completions(&mut self) {
    // -- Phase 0b — deferred completions (the RPC pattern, see
    //    `crate::rpc`): drain the worker reports (non-blocking — the
    //    room never awaits a worker; this is the same try_recv
    //    discipline as the control channel above) and reconcile each
    //    report against the pending set. Exactly one answer per
    //    request is structural: a report for an id that is no longer
    //    pending (already answered, timed out below, or its
    //    connection left) is dropped and counted.
    while let Ok(rep) = self.completions.try_recv() {
        let Some(deq) = self.pending.get_mut(&rep.conn) else {
            // The connection left (its pending set was cleared on
            // leave/rejoin): the report is stale by construction.
            self.m.requests_late += 1;
            continue;
        };
        match deq.iter().position(|p| p.id == rep.id) {
            Some(idx) => {
                // The inner op comes from the pending entry (the
                // worker's report is outcome-only — the room is the
                // authority on the request's shape).
                let op = deq[idx].op;
                deq.remove(idx);
                self.pending_total -= 1;
                if deq.is_empty() {
                    self.pending.remove(&rep.conn);
                }
                self.queued
                    .entry(rep.conn)
                    .or_default()
                    .push(crate::rpc::RpcReply {
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
    // Sweep expired pending requests (the client-visible timeout —
    // see `crate::rpc`): per connection the deadlines are
    // non-decreasing (same timeout, FIFO arrivals), so only the head
    // of each deque can be due. The room's tick body stays
    // synchronous: this is a wall-clock comparison, no await.
    // Cost when quiet: one `is_empty` probe (the common case — a
    // room with no pending requests pays nothing below it).
    if !self.pending.is_empty() {
        let now = Instant::now();
        // Pop the due heads (conn + request together — the reply is
        // owed to the request's owner), then queue the timeout
        // replies outside the borrow of `self.pending`.
        let mut due: Vec<(ConnectionId, crate::rpc::PendingRequest)> = Vec::new();
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
            self.queued.entry(conn).or_default().push(crate::rpc::RpcReply {
                id: p.id,
                ok: false,
                op: p.op,
                reason: crate::rpc::TIMEOUT_REASON.to_string(),
                payload: bytes::Bytes::new(),
            });
        }
    }

    }
}
