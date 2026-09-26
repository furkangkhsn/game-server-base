//! Phase 2c — the correlated requests.

use std::collections::VecDeque;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;

use crate::room::TickCtx;
use crate::rpc::{Completion, PendingRequest};

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
    /// Tick phase 2c — the correlated requests, the shard-side mirror of
    /// the room's.
    pub(crate) fn phase_requests(&mut self, requests: &[crate::rpc::RpcRequest], ctx: &TickCtx) {
        // -- Phase 2c — REQUESTS (the shard-side mirror of the room's
        //    request loop): duplicate-in-flight guard ABOVE the decision
        //    (every decision kind), then `handle_request`, then per-decision
        //    handling with caps enforced where the pending state lives.
        for req in requests {
            // A congested connection at its cap is refused before
            // anything else (the room's rule, F14).
            if self.refuses_congested(req.conn, req.player) {
                self.m.requests_refused_congested += 1;
                continue;
            }
            // Duplicate id still in flight: reject WITHOUT re-processing —
            // a second reply for one id would breach exactly-one-answer,
            // and a retrying client must not buy a double-applied side
            // effect. The id becomes reusable once answered (no history).
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
                    // Not a request this logic handles: a normal "no
                    // handler" rejection (the client learns immediately
                    // instead of waiting out its own timeout).
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
                    // Caps (this actor's authority on pending state): a
                    // request past a cap is a normal rejection, same tick —
                    // the logic's `External` decision does not commit the
                    // registration. Same fields as the room's (the shared
                    // RoomConfig), so one sizing derivation covers both
                    // actors. Priority mirrors the room: over BOTH caps
                    // counts against the per-connection bucket (the
                    // client's own quota is the actionable one).
                    let per_conn = self.pending.get(&req.conn).map(VecDeque::len).unwrap_or(0);
                    let over_conn_cap = per_conn >= self.config.max_pending_requests_per_conn;
                    let over_room_cap = self.pending_total >= self.config.max_pending_requests;
                    if over_conn_cap || over_room_cap {
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
                    // Register it pending on THIS shard, then delegate.
                    let due = Instant::now() + self.config.request_timeout;
                    self.pending
                        .entry(req.conn)
                        .or_default()
                        .push_back(PendingRequest {
                            id: req.id,
                            op: req.op,
                            due,
                        });
                    self.pending_total += 1;
                    self.m.requests_external += 1;
                    // The worker task: resolves the future under a timeout
                    // RESOURCE guard (the same deadline this actor's sweep
                    // enforces as the client-visible authority; on expiry
                    // the worker reports nothing). The report rides the
                    // completion channel; the 0b phase of a later tick
                    // reconciles it.
                    let conn = req.conn;
                    let id = req.id;
                    let timeout = self.config.request_timeout;
                    let report_tx = self.completions_tx.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(timeout, fut).await {
                            Ok(Ok(payload)) => {
                                let _ = report_tx.send(Completion::reply(conn, id, payload)).await;
                            }
                            Ok(Err(reason)) => {
                                let _ = report_tx.send(Completion::error(conn, id, reason)).await;
                            }
                            Err(_elapsed) => {
                                // Timed out: no report — the sweep owns the
                                // client-visible timeout. The worker exits.
                            }
                        }
                        // A failed report send means the shard shut down:
                        // the worker exits either way — its lifetime is
                        // bounded by the timeout in every case.
                    });
                }
            }
        }
    }

    /// The storm bound (F14; the room actor's `refuses_congested`, whose
    /// docs carry the rationale): a request from a congested connection
    /// (`RoomConn.dropping`) that already owes as many answers — queued,
    /// carried, or in flight — as its in-flight cap is refused, neither
    /// processed nor answered. Never true while batches go through.
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
            + self.pending.get(&conn).map_or(0, VecDeque::len);
        owed >= self.config.max_pending_requests_per_conn
    }
}
