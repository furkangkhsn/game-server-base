//! The request and call phases both tick actors run — written ONCE (the
//! room and the shard each used to carry a byte-for-byte copy of the
//! RPC machinery; `docs/TRAIT-ARCHITECTURE.md` Faz 3's debt). The state
//! itself is the actor's [`Calls`] table (pending calls, the completion
//! channel, the External deadlines); this module is the glue between it
//! and the actor's own tables (`queued`, `conns`, the counters).
//!
//! ```text
//! RESPONSES  responses()      table.responses → answers queued
//! READ       split() + read() envelopes decoded; a claimed op → the table
//! 2c         handle()         the rest → GameLogic::handle_request
//! FLUSH      (Calls::flush, after SYSTEMS)
//! BROADCAST  end_tick()       table.end_tick → answers queued
//! ```

use std::collections::HashMap;
use std::time::Instant;

use prost::Message;

use crate::calls::{Calls, Identity, Owner};
use crate::id::{ConnectionId, PlayerId};
use crate::room::{Action, GameLogic, RoomConfig, RoomConn, RoomCounters, TickCtx};
use crate::rpc::{RequestDecision, RpcReply, RpcRequest};

/// [`Parts`] of an actor (`self`): its table, counters, answer queue,
/// members and config — field by field, so its logic and world stay
/// free.
macro_rules! parts {
    ($s:ident) => {
        $crate::room::Parts {
            calls: &mut $s.calls,
            m: &mut $s.m,
            queued: &mut $s.queued,
            conns: &$s.conns,
            config: &$s.config,
        }
    };
}
pub(crate) use parts;

/// The call table of an actor whose actor id is `actor` (a room id, a
/// shard's sample id), sized by `config`, with the logic's boot
/// registrations (`GameLogic::boot_calls`). Its key origin derives from
/// the actor's `seed` (`mix(seed ^ mix(actor))`, `docs/SERVICES.md`
/// §10.1): a replay of the same seed issues the same keys; a fresh seed
/// per incarnation keeps them unique across rebirths. A registration
/// error is logged and handed back (the first claim of an op stays): the
/// actor is built anyway — its construction is infallible — and keeps the
/// error (`boot_error`) for the registry, which refuses to install a room
/// whose table did not boot (B153).
pub(crate) fn table<W, G, Sp>(
    config: &RoomConfig,
    actor: u64,
    seed: u64,
    logic: &mut dyn GameLogic<W, GroupKey = G, Strip = Sp>,
) -> (Calls, Option<crate::calls::BootError>)
where
    G: Eq + std::hash::Hash + Clone + std::fmt::Debug,
    Sp: std::fmt::Debug + Clone + PartialEq + Send + 'static,
{
    let mut calls = Calls::new(crate::calls::CallsConfig {
        actor: crate::calls::ActorId(actor),
        origin: crate::calls::mix(seed ^ crate::calls::mix(actor)),
        share: config.call_share,
        completions: config
            .call_completions
            .saturating_add(config.max_pending_requests)
            .max(1),
        timeout: config.call_timeout,
    });
    let boot = logic.boot_calls(&mut calls).err();
    if let Some(e) = &boot {
        tracing::error!(room = %config.id, error = %e, "call table boot registration failed");
    }
    (calls, boot)
}

/// The event table of an actor whose call table is `calls` (booted
/// first: requests and events share one op space), with the logic's
/// registrations (`GameLogic::boot_events`), behind its own bounded
/// mailbox (`RoomConfig::event_capacity`). A registration error is
/// logged and handed back like the call table's (B153: the registry
/// refuses the room).
pub(crate) fn event_inbound<W, G, Sp>(
    config: &RoomConfig,
    calls: &Calls,
    logic: &mut dyn GameLogic<W, GroupKey = G, Strip = Sp>,
) -> (crate::events::Inbound, Option<crate::calls::BootError>)
where
    G: Eq + std::hash::Hash + Clone + std::fmt::Debug,
    Sp: std::fmt::Debug + Clone + PartialEq + Send + 'static,
{
    let mut events = crate::events::Events::sharing_ops(calls);
    let boot = logic.boot_events(&mut events).err();
    if let Some(e) = &boot {
        tracing::error!(room = %config.id, error = %e, "event table boot registration failed");
    }
    (
        crate::events::Inbound::new(config.event_capacity, events),
        boot,
    )
}

/// The actor state the request phases touch, borrowed field by field (so
/// the logic and the world stay free for the caller).
pub(crate) struct Parts<'a, G> {
    pub(crate) calls: &'a mut Calls,
    pub(crate) m: &'a mut RoomCounters,
    pub(crate) queued: &'a mut HashMap<ConnectionId, Vec<RpcReply>>,
    pub(crate) conns: &'a HashMap<PlayerId, RoomConn<G>>,
    pub(crate) config: &'a RoomConfig,
}

impl<G> Parts<'_, G> {
    fn queue(&mut self, conn: ConnectionId, reply: RpcReply) {
        self.queued.entry(conn).or_default().push(reply);
    }

    fn reject(&mut self, conn: ConnectionId, id: u64, op: u16, reason: String) {
        self.queue(conn, RpcReply::rejected(id, op, reason));
    }

    /// Queue the table's answers (engine answers, replies, `.answers`,
    /// External reports) behind the session's earlier ones. A session
    /// that ended meanwhile has no row: the broadcast counts its answers
    /// `requests_undelivered` (B53).
    pub(crate) fn drain_outbox(&mut self) {
        for (conn, reply) in self.calls.take_outbox() {
            self.queue(conn, reply);
        }
    }

    /// RESPONSES: the finished calls (refusals of the last FLUSH, the
    /// completion channel, the External deadline sweep on the tick clock
    /// `now`). `alive` says whether a call's owner still exists.
    pub(crate) fn responses(&mut self, now: Instant, alive: &dyn Fn(Owner) -> bool) {
        self.calls.responses(now, alive, self.m);
        self.drain_outbox();
    }

    /// BROADCAST: the table closes what nobody took or answered.
    pub(crate) fn end_tick(&mut self) {
        self.calls.end_tick();
        self.drain_outbox();
    }

    /// A session ended: its request state goes — the answers queued for
    /// it (`requests_undelivered`) and every answer the table owed it
    /// (`requests_abandoned`, B53).
    pub(crate) fn drop_session(&mut self, conn: ConnectionId) {
        #[cfg(feature = "record")]
        self.calls
            .note(|| crate::determinism::Input::SessionEnd { conn });
        self.calls.drop_session(conn, self.m);
        if let Some(owed) = self.queued.remove(&conn) {
            self.m.requests_undelivered += owed.len() as u64;
        }
    }

    /// A session parked (detach-hold): its answers go as on
    /// [`Self::drop_session`], its explicit binding stays for the resume
    /// (B156).
    pub(crate) fn park_session(&mut self, conn: ConnectionId) {
        #[cfg(feature = "record")]
        self.calls
            .note(|| crate::determinism::Input::SessionPark { conn });
        self.calls.park_session(conn, self.m);
        if let Some(owed) = self.queued.remove(&conn) {
            self.m.requests_undelivered += owed.len() as u64;
        }
    }

    /// The storm bound (F14): whether a request from `conn` must be
    /// refused — not processed, not answered — because the connection is
    /// congested (its latest batch was dropped: `RoomConn.dropping`) and
    /// already owes as many answers as its in-flight cap
    /// (`max_pending_requests_per_conn`), counting undelivered answers
    /// (`queued`, the carried ones included) and the answers the table
    /// owes it (open requests, calls that answer it, External requests in
    /// flight) alike. Refusing is the only bounded choice: any answer, a
    /// rejection included, is one more undelivered answer. The client's
    /// own timeout covers a refused request, and nothing was applied, so
    /// a retry is safe. Never true for a connection whose batches go
    /// through: nothing changes when nothing is dropped.
    pub(crate) fn refuses_congested(&self, conn: ConnectionId, player: PlayerId) -> bool {
        let congested = self
            .conns
            .get(&player)
            .is_some_and(|rc| rc.dropping && rc.conn == conn);
        if !congested {
            return false;
        }
        let owed = self.queued.get(&conn).map_or(0, Vec::len) + self.calls.owed(conn);
        owed >= self.config.max_pending_requests_per_conn
    }

    /// Phase 2a — split the requests out of the pulled actions (the RPC
    /// pattern, see `crate::rpc`). A request is an action carrying the
    /// base-band envelope opcode; the core decodes the envelope (a base
    /// message). The split is in-order: both the actions and the
    /// requests keep their arrival order. Cost when quiet: one `u16`
    /// compare per pulled action.
    ///
    /// A malformed envelope is a *normal rejection* (a client bug — the
    /// same class as an undecodable game payload), not a protocol
    /// violation: answered with a reject and counted. `id = 0` is
    /// rejected the same way (it cannot correlate).
    pub(crate) fn split(&mut self, actions: &mut Vec<Action>) -> Vec<RpcRequest> {
        let mut requests: Vec<RpcRequest> = Vec::new();
        actions.retain_mut(|a| {
            if a.op != crate::rpc::RPC_REQ_OP {
                return true;
            }
            match gsb_protocol::base::RpcRequest::decode(&a.payload[..]) {
                Ok(env) if env.id != 0 => {
                    requests.push(RpcRequest {
                        conn: a.conn,
                        // Already translated (phase 1.5): the logic sees
                        // the stable player key, while the answer state
                        // stays session-scoped under `conn`.
                        player: a.player,
                        id: env.id,
                        // The wire type is `u32` (proto3 has no 16-bit
                        // integers); the op space is `u16` by protocol
                        // contract (a value above it decodes to the `u16`
                        // truncation, an op nobody handles).
                        op: env.op as u16,
                        payload: env.payload.into(),
                    });
                    false
                }
                // Its answer would be owed too: a congested connection at
                // its cap is refused here, like at READ/2c (F14).
                _ if self.refuses_congested(a.conn, a.player) => {
                    self.m.requests_refused_congested += 1;
                    false
                }
                _ => {
                    self.m.requests_rejected_malformed += 1;
                    self.reject(a.conn, 0, a.op, crate::rpc::MALFORMED_REASON.to_string());
                    false
                }
            }
        });
        requests
    }

    /// READ — the requests whose op the table claims (`accept`,
    /// `forward`) go into the table now, in arrival order, past the two
    /// gates every request meets (the storm bound, then the in-flight
    /// duplicate); the rest stay in `requests` for `handle_request` (2c).
    /// `owner_of` is the session's default owner
    /// (`GameLogic::call_owner`). Cost when the table claims nothing:
    /// one `is_empty`.
    pub(crate) fn read(
        &mut self,
        requests: &mut Vec<RpcRequest>,
        owner_of: &dyn Fn(PlayerId) -> Option<Owner>,
    ) {
        if requests.is_empty() || !self.calls.claims_any() {
            return;
        }
        let mut rest = Vec::with_capacity(requests.len());
        for req in requests.drain(..) {
            if !self.calls.handles(req.op) {
                rest.push(req);
                continue;
            }
            if self.refuses_congested(req.conn, req.player) {
                self.m.requests_refused_congested += 1;
                continue;
            }
            if self.calls.is_live(req.conn, req.id) {
                self.m.requests_rejected_dup += 1;
                self.reject(
                    req.conn,
                    req.id,
                    req.op,
                    crate::rpc::DUPLICATE_REASON.to_string(),
                );
                continue;
            }
            let from = match self.conns.get(&req.player) {
                Some(rc) => Identity::new(&rc.identity),
                None => Identity::new(""),
            };
            let owner = owner_of(req.player);
            self.calls.read(
                req.conn,
                req.player,
                &from,
                owner,
                req.id,
                req.op,
                &req.payload,
            );
        }
        *requests = rest;
        self.drain_outbox();
    }

    /// Phase 2c — the requests no table rule claims: registration, the
    /// pending caps, and the built-in call behind an `External` decision.
    /// Synchronous: an `External` decision returns an owning future that
    /// a spawned worker resolves off the tick (`Calls::external`).
    pub(crate) fn handle<W, Sp>(
        &mut self,
        requests: &[RpcRequest],
        logic: &mut dyn GameLogic<W, GroupKey = G, Strip = Sp>,
        world: &mut W,
        ctx: &TickCtx,
    ) where
        G: Eq + std::hash::Hash + Clone + std::fmt::Debug,
        Sp: std::fmt::Debug + Clone + PartialEq + Send + 'static,
    {
        for req in requests {
            // A congested connection at its cap is refused before
            // anything else (F14 — see `refuses_congested`).
            if self.refuses_congested(req.conn, req.player) {
                self.m.requests_refused_congested += 1;
                continue;
            }
            #[cfg(feature = "record")]
            self.calls.note(|| crate::determinism::Input::Request {
                conn: req.conn,
                player: req.player,
                id: req.id,
                op: req.op,
                payload: req.payload.clone(),
            });
            // Duplicate id that is still in flight: reject WITHOUT
            // re-processing — for every decision kind, not just external.
            // An in-flight id is already correlated with a pending
            // request: a second request under the same id (even a
            // room-local one, which would be answered in this very tick)
            // would produce a second reply for one id, and a retrying
            // client must not be able to buy a double-applied side effect
            // (see the `rpc` module docs). Normal rejection, same tick;
            // the id becomes reusable once the first request is answered
            // (no unbounded history).
            if self.calls.is_live(req.conn, req.id) {
                self.m.requests_rejected_dup += 1;
                self.reject(
                    req.conn,
                    req.id,
                    req.op,
                    crate::rpc::DUPLICATE_REASON.to_string(),
                );
                continue;
            }
            match logic.handle_request(world, ctx, req) {
                None => {
                    // Not a request this logic handles: a normal "no
                    // handler" rejection (the client learns it instead
                    // of waiting for its own timeout).
                    self.m.requests_rejected_no_handler += 1;
                    self.reject(
                        req.conn,
                        req.id,
                        req.op,
                        crate::rpc::no_handler_reason(req.op),
                    );
                }
                Some(RequestDecision::Reply(payload)) => {
                    self.m.requests_local += 1;
                    self.queue(req.conn, RpcReply::ok(req.id, req.op, payload));
                }
                Some(RequestDecision::Reject(reason)) => {
                    self.m.requests_rejected_logic += 1;
                    self.reject(req.conn, req.id, req.op, reason);
                }
                Some(RequestDecision::External(fut)) => {
                    // Caps (the actor's authority on pending state): a
                    // request past a cap is a normal rejection, same tick
                    // — the logic's `External` decision does not commit
                    // the actor to registering it. A request past both
                    // caps counts against the per-connection one (the
                    // client's own quota is the actionable one).
                    let over_conn_cap = self.calls.external_in_flight(req.conn)
                        >= self.config.max_pending_requests_per_conn;
                    let over_room_cap =
                        self.calls.external_total() >= self.config.max_pending_requests;
                    if over_conn_cap {
                        self.m.requests_rejected_conn_cap += 1;
                        self.reject(
                            req.conn,
                            req.id,
                            req.op,
                            crate::rpc::CONN_CAP_REASON.to_string(),
                        );
                        continue;
                    }
                    if over_room_cap {
                        self.m.requests_rejected_room_cap += 1;
                        self.reject(
                            req.conn,
                            req.id,
                            req.op,
                            crate::rpc::ROOM_CAP_REASON.to_string(),
                        );
                        continue;
                    }
                    // Registered in flight, then delegated: the built-in
                    // call (its deadline on the tick clock — F16 — owned
                    // by the RESPONSES sweep; the worker's own timeout
                    // only bounds the task).
                    let timeout = self.config.request_timeout;
                    let due = self.calls.now() + timeout;
                    self.calls
                        .external(req.conn, req.id, req.op, fut, due, timeout);
                    self.m.requests_external += 1;
                }
            }
        }
    }
}
