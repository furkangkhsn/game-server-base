//! Phase 2c — the correlated requests.

use crate::room::*;
use std::fmt::Debug;
use std::hash::Hash;

use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Tick phase 2c — the correlated requests: registration, the
    /// pending caps, and the worker spawn for the deferred ones.
    pub(super) fn phase_requests(&mut self, requests: &[crate::rpc::RpcRequest], ctx: &TickCtx) {
        // -- Phase 2c — REQUESTS: the correlated requests (see `rpc`).
        //    Synchronous: an `External` decision returns an owning
        //    future that a spawned worker resolves off the tick; the
        //    tick body only registers the request and (for the deferred
        //    ones) spawns the worker.
        for req in requests {
            // A congested connection at its cap is refused before
            // anything else (F14 — see `refuses_congested`).
            if self.refuses_congested(req.conn, req.player) {
                self.m.requests_refused_congested += 1;
                continue;
            }
            // Duplicate id that is still in flight: reject WITHOUT
            // re-processing — for every decision kind, not just external.
            // An in-flight id is already correlated with a pending
            // request: a second request under the same id (even a
            // room-local one, which would be answered in this very tick)
            // would produce a second reply for one id, and a retrying
            // client must not be able to buy a double-applied side
            // effect (see the `rpc` module docs). Normal rejection, same
            // tick; the id becomes reusable once the first request is
            // answered (no unbounded history).
            if self
                .pending
                .get(&req.conn)
                .is_some_and(|d| d.iter().any(|p| p.id == req.id))
            {
                self.m.requests_rejected_dup += 1;
                self.queue_reply(
                    req.conn,
                    req.id,
                    req.op,
                    false,
                    "duplicate request id (the request is still in flight)".to_string(),
                    bytes::Bytes::new(),
                );
                continue;
            }
            let decision = self.logic.handle_request(&mut self.world, ctx, req);
            match decision {
                None => {
                    // Not a request this logic handles: answer with a
                    // normal rejection (the client learns "no handler"
                    // instead of waiting for its own timeout).
                    self.m.requests_rejected_no_handler += 1;
                    self.queue_reply(
                        req.conn,
                        req.id,
                        req.op,
                        false,
                        format!("no request handler for op {:#04x}", req.op),
                        bytes::Bytes::new(),
                    );
                }
                Some(crate::rpc::RequestDecision::Reply(payload)) => {
                    self.m.requests_local += 1;
                    self.queue_reply(req.conn, req.id, req.op, true, String::new(), payload);
                }
                Some(crate::rpc::RequestDecision::Reject(reason)) => {
                    self.m.requests_rejected_logic += 1;
                    self.queue_reply(req.conn, req.id, req.op, false, reason, bytes::Bytes::new());
                }
                Some(crate::rpc::RequestDecision::External(fut)) => {
                    // Caps (the room's authority on pending state): a
                    // request past a cap is a normal rejection, same
                    // tick — the logic's `External` decision does not
                    // commit the room to registering it. (The duplicate
                    // check is above the decision: it applies to every
                    // kind.)
                    let per_conn = self
                        .pending
                        .get(&req.conn)
                        .map(std::collections::VecDeque::len)
                        .unwrap_or(0);
                    let over_conn_cap = per_conn >= self.config.max_pending_requests_per_conn;
                    let over_room_cap = self.pending_total >= self.config.max_pending_requests;
                    if over_conn_cap || over_room_cap {
                        // A request past both caps counts against the
                        // per-connection one (the reply names it first —
                        // the client's own quota is the actionable one).
                        if over_conn_cap {
                            self.m.requests_rejected_conn_cap += 1;
                        } else {
                            self.m.requests_rejected_room_cap += 1;
                        }
                        let reason = if over_conn_cap {
                            "pending request limit reached (per connection)".to_string()
                        } else {
                            "pending request limit reached (room)".to_string()
                        };
                        self.queue_reply(
                            req.conn,
                            req.id,
                            req.op,
                            false,
                            reason,
                            bytes::Bytes::new(),
                        );
                        continue;
                    }
                    // Register it pending, then delegate.
                    let due = crate::ticker::now() + self.config.request_timeout;
                    self.pending.entry(req.conn).or_default().push_back(
                        crate::rpc::PendingRequest {
                            id: req.id,
                            op: req.op,
                            due,
                        },
                    );
                    self.pending_total += 1;
                    self.m.requests_external += 1;
                    // The worker: resolves the future (or gives up at the
                    // same deadline the room's sweep enforces — the
                    // worker's timeout is a resource guard, the room's
                    // sweep is the client-visible authority; on expiry
                    // the worker reports nothing). The report rides the
                    // completion channel; the room reconciles it on a
                    // later tick's CONTROL phase.
                    let conn = req.conn;
                    let id = req.id;
                    let timeout = self.config.request_timeout;
                    let report_tx = self.completions_tx.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(timeout, fut).await {
                            Ok(Ok(payload)) => {
                                let _ = report_tx
                                    .send(crate::rpc::Completion::reply(conn, id, payload))
                                    .await;
                            }
                            Ok(Err(reason)) => {
                                let _ = report_tx
                                    .send(crate::rpc::Completion::error(conn, id, reason))
                                    .await;
                            }
                            Err(_elapsed) => {
                                // Timed out: no report — the room's sweep
                                // owns the client-visible timeout (it may
                                // have answered it already, on this or a
                                // previous tick). The worker exits.
                            }
                        }
                        // The report send fails (the channel is closed)
                        // when the room shut down: the worker exits
                        // either way — its lifetime is bounded by the
                        // timeout in every case (no task outlives a
                        // request by more than the timeout).
                    });
                }
            }
        }
    }

    /// The storm bound (F14): whether a request from `conn` must be
    /// refused — not processed, not answered — because the connection is
    /// congested (its latest batch was dropped: `RoomConn.dropping`) and
    /// already owes as many answers as its in-flight cap
    /// (`max_pending_requests_per_conn`), counting undelivered answers
    /// (`queued`, the carried ones included) and in-flight requests
    /// (`pending`) alike. Refusing is the only bounded choice: any answer,
    /// a rejection included, is one more undelivered answer. The client's
    /// own timeout covers a refused request, and nothing was applied, so a
    /// retry is safe. Checked where accepting would owe an answer (every
    /// request, before the duplicate check; a malformed envelope in 2a),
    /// so while congested what a connection owes never grows past the
    /// larger of the cap and what it owed when the run began — itself at
    /// most the cap plus one tick's pull (`max_actions_per_conn_per_tick`).
    /// Never true for a connection whose batches go through: nothing
    /// changes when nothing is dropped.
    pub(super) fn refuses_congested(
        &self,
        conn: crate::id::ConnectionId,
        player: crate::id::PlayerId,
    ) -> bool {
        let congested = self
            .conns
            .get(&player)
            .is_some_and(|rc| rc.dropping && rc.conn == conn);
        if !congested {
            return false;
        }
        let owed = self.queued.get(&conn).map_or(0, Vec::len)
            + self
                .pending
                .get(&conn)
                .map_or(0, std::collections::VecDeque::len);
        owed >= self.config.max_pending_requests_per_conn
    }
}
