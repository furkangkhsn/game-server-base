//! The table's drain points — what the room and the shard actor call at
//! each phase (SERVICES.md §3):
//!
//! ```text
//! CONTROL    drop_session (a session ended: unbind, abandon what it was owed;
//!            park_session/resume_session keep an explicit bind across a
//!            detach-hold)
//! RESPONSES  responses   (last FLUSH's refusals, the completion channel,
//!                         the game calls' deadline sweep, the built-in
//!                         External deadline sweep)
//! READ       read        (a request of a claimed op → the table)
//! SYSTEMS    the game    (take_incoming / call / take_resolved)
//! FLUSH      flush       (this tick's calls leave, in issue order)
//! BROADCAST  end_tick    (close what nobody took or answered)
//! ```
//!
//! Plus the built-in path of `RequestDecision::External` ([`Calls::external`]):
//! the legacy decision becomes a call the table keeps like any other —
//! one pending map, one completion channel, one answer path — counted on
//! the request counters it always had (`requests_external`,
//! `requests_timed_out`, `requests_late`, `requests_abandoned`).

use std::any::TypeId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::calls::contract::{
    Call, CallCx, CallError, CallKey, Caller, Ended, Erased, Failure, Identity, Outcome, Owner,
    Refusal,
};
use crate::calls::route::{Completion, ReplySink, SendFail, ServiceId, ServiceStats, SinkTo};
use crate::calls::table::{Answer, Calls, Entry, OpenReq, Slot};
use crate::id::{ConnectionId, PlayerId};
use crate::room::RoomCounters;
use crate::rpc::{self, RpcReply};

/// The built-in call behind `RequestDecision::External`: the logic's
/// future, answered with the bytes (or the reason) it resolves to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct External;

impl Call for External {
    type Response = Bytes;
    type Error = String;
}

/// The built-in External call's type name in an input log (its late
/// answers count on the request counters).
#[cfg(feature = "record")]
pub(crate) const EXTERNAL: &str = "gsb::External";

/// The future an `External` decision hands over.
pub(crate) type ExternalFuture = Pin<Box<dyn Future<Output = Result<Bytes, String>> + Send>>;

/// The legacy `External` decision's answer: its `Err(reason)` is a
/// text rejection (an engine-shaped answer, `game_error = false`), as it
/// always was.
fn encode_external(o: &Erased, id: u64, op: u16) -> RpcReply {
    match o.downcast_ref::<Outcome<External>>() {
        Some(Ok(b)) => RpcReply::ok(id, op, b.clone()),
        Some(Err(CallError::Service(reason))) => RpcReply::rejected(id, op, reason.clone()),
        Some(Err(CallError::TimedOut)) => RpcReply::rejected(id, op, rpc::TIMEOUT_REASON),
        _ => RpcReply::rejected(id, op, rpc::ABANDONED_REASON),
    }
}

fn no_deliver(
    _: &mut crate::calls::table::Buckets,
    _: CallKey,
    _: Owner,
    _: Box<dyn std::any::Any + Send + Sync>,
    _: Erased,
) {
}

impl Calls {
    // ---- RESPONSES -----------------------------------------------------------

    /// RESPONSES: absorb finished calls — the refusals the last FLUSH
    /// decided first, then the completion channel in arrival order, then
    /// the game calls' own deadline (`CallsConfig::timeout`), then the
    /// built-in `External` deadlines — both on the tick clock (`now`).
    /// `alive` says whether an owner still exists. Cost when quiet: one
    /// empty `try_recv` and three `is_empty` probes.
    pub(crate) fn responses(
        &mut self,
        now: Instant,
        alive: &dyn Fn(Owner) -> bool,
        m: &mut RoomCounters,
    ) {
        if !self.local.is_empty() {
            for c in std::mem::take(&mut self.local) {
                self.absorb(c.key, c.outcome, alive, m);
            }
        }
        while let Ok(c) = self.rx.try_recv() {
            self.absorb(c.key, c.outcome, alive, m);
        }
        if !self.call_deadlines.is_empty() {
            self.sweep_calls(now, alive);
        }
        // The built-in deadline (the client-visible timeout of an
        // External request — `crate::rpc`): every one shares the room's
        // timeout and `now` never goes back, so the queue is in deadline
        // order and only its head can be due. A head whose call already
        // ended is skipped.
        while let Some(&(due, key)) = self.deadlines.front() {
            if due > now {
                break;
            }
            self.deadlines.pop_front();
            #[cfg(feature = "record")]
            if self.pending.contains_key(&key) {
                let out: Outcome<External> = Err(CallError::TimedOut);
                let out: Erased = Box::new(out);
                self.note_ending(
                    EXTERNAL,
                    crate::calls::contract::copy_outcome::<External>,
                    &out,
                    crate::determinism::Arrival::Deadline,
                    false,
                    key,
                );
            }
            self.expire_external(key, m);
        }
        #[cfg(feature = "record")]
        self.note_responses();
    }

    /// An External request's deadline passed: answer its client
    /// `TimedOut` (a no-op when it already ended).
    pub(crate) fn expire_external(&mut self, key: CallKey, m: &mut RoomCounters) {
        if let Some(e) = self.pending.remove(&key) {
            self.untrack(&e);
            m.requests_timed_out += 1;
            if let Some(a) = &e.answer {
                let out: Outcome<External> = Err(CallError::TimedOut);
                let out: Erased = Box::new(out);
                self.answer_now(a.conn, a.wire_id, a.op, a.encode, &out);
            }
        }
    }

    fn absorb(
        &mut self,
        key: CallKey,
        outcome: Erased,
        alive: &dyn Fn(Owner) -> bool,
        m: &mut RoomCounters,
    ) {
        let Some(e) = self.pending.remove(&key) else {
            // A key this table no longer holds: an External report after
            // its deadline or its session's end, an answer for a call
            // exported away, or one the table's own deadline ended.
            let external = outcome.is::<Outcome<External>>();
            if external {
                m.requests_late += 1;
            } else {
                self.counts.results_late += 1;
            }
            #[cfg(feature = "record")]
            self.note_late(key, external);
            return;
        };
        self.finish(key, e, outcome, alive);
    }

    /// A game call ended (`e` already out of `pending`): count its
    /// ending, answer its client, hand its result to the game.
    pub(crate) fn finish(
        &mut self,
        key: CallKey,
        e: Entry,
        outcome: Erased,
        alive: &dyn Fn(Owner) -> bool,
    ) {
        #[cfg(feature = "record")]
        if self.recording() {
            let lives = e.owner == Owner::ACTOR || alive(e.owner);
            self.note_ending(
                e.call,
                e.copy,
                &outcome,
                crate::determinism::Arrival::Ended,
                lives,
                key,
            );
        }
        self.untrack(&e);
        if e.legacy {
            if let Some(a) = &e.answer {
                self.answer_now(a.conn, a.wire_id, a.op, a.encode, &outcome);
            }
            return;
        }
        match (e.ended)(&outcome) {
            Ended::Ok => self.counts.results_ok += 1,
            Ended::ServiceError => self.counts.results_service_error += 1,
            Ended::Refused => self.counts.results_refused += 1,
            Ended::TimedOut => self.counts.results_timed_out += 1,
            Ended::Abandoned => self.counts.results_abandoned += 1,
        }
        if let Some(a) = &e.answer {
            self.answer_now(a.conn, a.wire_id, a.op, a.encode, &outcome);
            self.counts.answers_sent += 1;
        }
        if let Some(ctx) = e.ctx {
            if e.owner != Owner::ACTOR && !alive(e.owner) {
                self.counts.responses_owner_gone += 1;
            } else {
                (e.deliver)(&mut self.buckets, key, e.owner, ctx, outcome);
                self.waiting += 1;
                self.counts.responses_delivered += 1;
            }
        }
    }

    /// The table's own deadline for game calls (`CallsConfig::timeout`):
    /// a sent call still pending past it ends `TimedOut` here, like any
    /// other ending (the service's later answer is `results_late`).
    /// Every call shares the timeout and FLUSH stamps on the tick clock,
    /// so the queue is in deadline order; a head whose call already
    /// ended (or left: export) is skipped.
    fn sweep_calls(&mut self, now: Instant, alive: &dyn Fn(Owner) -> bool) {
        while let Some(&(due, key)) = self.call_deadlines.front() {
            if due > now {
                break;
            }
            self.call_deadlines.pop_front();
            if let Some(e) = self.pending.remove(&key) {
                let outcome = (e.fail)(Failure::TimedOut);
                self.finish(key, e, outcome, alive);
            }
        }
    }

    // ---- READ ------------------------------------------------------------------

    /// Is `id` owed an answer on `conn` (the duplicate gate of both
    /// request paths)?
    pub(crate) fn is_live(&self, conn: ConnectionId, id: u64) -> bool {
        self.live.contains(&(conn, id))
    }

    /// How many answers `conn` is owed by the table (open requests, calls
    /// that answer it, External requests in flight) — the storm bound's
    /// in-flight half.
    pub(crate) fn owed(&self, conn: ConnectionId) -> usize {
        self.owed.get(&conn).copied().unwrap_or(0)
    }

    /// READ: one request of an op the table claims (the gates — malformed
    /// envelope, congestion, duplicate id — already passed). `owner` is
    /// the session's default owner (`GameLogic::call_owner`); an explicit
    /// `bind` overrides it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn read(
        &mut self,
        conn: ConnectionId,
        player: PlayerId,
        from: &Identity,
        owner: Option<Owner>,
        wire_id: u64,
        op: u16,
        payload: &[u8],
    ) {
        #[cfg(feature = "record")]
        self.note(|| crate::determinism::Input::Read {
            conn,
            player,
            from: from.clone(),
            owner,
            wire_id,
            op,
            payload: Bytes::copy_from_slice(payload),
        });
        if let Some(rule) = self.forwards.get(&op).cloned() {
            match rule(self, conn, from, wire_id, payload) {
                Ok(()) => {
                    self.live_insert(conn, wire_id);
                    self.counts.requests_forwarded += 1;
                }
                Err(_) => {
                    self.counts.requests_undecodable += 1;
                    self.reject(conn, wire_id, op, rpc::UNDECODABLE_REASON);
                }
            }
            return;
        }
        let Some(&(type_id, decode)) = self.accepts.get(&op) else {
            debug_assert!(false, "read() is only called for a claimed op");
            return;
        };
        let Some(owner) = self.bound.get(&conn).copied().or(owner) else {
            self.counts.requests_unbound += 1;
            self.reject(conn, wire_id, op, rpc::NOT_BOUND_REASON);
            return;
        };
        self.next_open += 1;
        let open = self.next_open;
        match decode(payload, conn, player, from.clone(), owner, open) {
            Ok(item) => {
                self.open.insert(
                    open,
                    OpenReq {
                        conn,
                        wire_id,
                        op,
                        from: from.clone(),
                    },
                );
                self.live_insert(conn, wire_id);
                self.incoming.push(Slot {
                    type_id,
                    op,
                    open,
                    conn,
                    item,
                });
            }
            Err(_) => {
                self.counts.requests_undecodable += 1;
                self.reject(conn, wire_id, op, rpc::UNDECODABLE_REASON);
            }
        }
    }

    fn reject(&mut self, conn: ConnectionId, id: u64, op: u16, why: &str) {
        self.outbox.push((conn, RpcReply::rejected(id, op, why)));
    }

    // ---- the built-in External path ---------------------------------------------

    /// External requests in flight for `conn` (the per-connection cap).
    pub(crate) fn external_in_flight(&self, conn: ConnectionId) -> usize {
        self.legacy_by_conn.get(&conn).copied().unwrap_or(0)
    }

    /// External requests in flight (the room-wide cap, the
    /// `pending_requests` gauge).
    pub(crate) fn external_total(&self) -> usize {
        self.legacy_total
    }

    /// `RequestDecision::External` as a built-in call (the caps already
    /// checked): registered in flight now, its future resolved by a
    /// worker task bounded by `timeout` (a resource guard — the
    /// client-visible deadline `due` is the RESPONSES sweep's, on the tick
    /// clock), its report taken from the table's completion channel.
    pub(crate) fn external(
        &mut self,
        conn: ConnectionId,
        wire_id: u64,
        op: u16,
        fut: ExternalFuture,
        due: Instant,
        timeout: Duration,
    ) {
        self.seq += 1;
        let key = CallKey {
            origin: self.cfg.origin,
            seq: self.seq,
        };
        let e = Entry {
            key: Some(key),
            type_id: TypeId::of::<External>(),
            owner: Owner::ACTOR,
            service: ServiceId(0),
            stats: Arc::new(ServiceStats::default()),
            send: None,
            player: None,
            ctx: None,
            deliver: no_deliver,
            answer: Some(Answer {
                conn,
                wire_id,
                op,
                encode: encode_external,
            }),
            fail: crate::calls::contract::fail_for::<External>,
            ended: crate::calls::contract::ended_of::<External>,
            sent: true,
            legacy: true,
            #[cfg(feature = "record")]
            call: EXTERNAL,
            #[cfg(feature = "record")]
            copy: crate::calls::contract::copy_outcome::<External>,
        };
        *self.in_flight_by.entry((e.type_id, e.owner)).or_insert(0) += 1;
        *self.legacy_by_conn.entry(conn).or_insert(0) += 1;
        self.legacy_total += 1;
        self.live_insert(conn, wire_id);
        self.deadlines.push_back((due, key));
        self.pending.insert(key, e);
        // REPLAY: the future is not run — its ending is in the log.
        #[cfg(feature = "record")]
        if self.replay {
            return;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // Timed out: no report — the RESPONSES sweep owns the
            // client-visible timeout. Otherwise the report may wait for a
            // slot (this task parks; the actor never does); a closed
            // channel (the actor stopped) ends the worker either way.
            if let Ok(r) = tokio::time::timeout(timeout, fut).await {
                let out: Outcome<External> = r.map_err(CallError::Service);
                let _ = tx
                    .send(Completion {
                        key,
                        outcome: Box::new(out),
                    })
                    .await;
            }
        });
    }

    // ---- FLUSH -------------------------------------------------------------------

    /// After SYSTEMS: send this tick's calls, in issue order. A refusal
    /// is decided here and delivered at the next RESPONSES, like any
    /// other ending. Cost when quiet: one `is_empty`.
    pub(crate) fn flush(&mut self) {
        if self.drafts.is_empty() {
            return;
        }
        for e in std::mem::take(&mut self.drafts) {
            self.issue(e);
        }
    }

    fn issue(&mut self, mut e: Entry) {
        let key = match e.key {
            Some(k) => k,
            None => {
                self.seq += 1;
                CallKey {
                    origin: self.cfg.origin,
                    seq: self.seq,
                }
            }
        };
        e.key = Some(key);
        // REPLAY: the services are not called — the call waits under its
        // key for its recorded ending.
        #[cfg(feature = "record")]
        if self.replay {
            self.pending.insert(key, e);
            return;
        }
        let used = self.per_service.get(&e.service).copied().unwrap_or(0);
        if used >= self.cfg.share {
            self.counts.calls_refused_share += 1;
            self.local.push(Completion {
                key,
                outcome: (e.fail)(Failure::Refused(Refusal::Share)),
            });
            self.pending.insert(key, e);
            return;
        }
        let cx = CallCx {
            key,
            caller: Caller {
                actor: self.cfg.actor,
                player: e.player.clone(),
            },
            parent: None,
        };
        let sink = ReplySink {
            key,
            to: SinkTo::Actor(self.tx.clone()),
            fail: e.fail,
            stats: Arc::clone(&e.stats),
            quiet: false,
        };
        let sent = match &e.send {
            Some(send) => send(cx, sink),
            None => Err((SendFail::Gone, sink)),
        };
        match sent {
            Ok(()) => {
                e.sent = true;
                *self.per_service.entry(e.service).or_insert(0) += 1;
                self.counts.calls_sent += 1;
                if let Some(t) = self.cfg.timeout {
                    self.call_deadlines
                        .push_back((crate::ticker::now() + t, key));
                }
            }
            Err((why, sink)) => {
                sink.disarm();
                let r = match why {
                    SendFail::Full => {
                        self.counts.calls_refused_full += 1;
                        Refusal::Full
                    }
                    SendFail::Gone => {
                        self.counts.calls_refused_gone += 1;
                        Refusal::Gone
                    }
                };
                self.local.push(Completion {
                    key,
                    outcome: (e.fail)(Failure::Refused(r)),
                });
            }
        }
        self.pending.insert(key, e);
    }

    // ---- tick end -------------------------------------------------------------------

    /// BROADCAST: close what nobody took or answered (counted; the client
    /// gets an engine answer) and drop the results nobody took. Cost when
    /// quiet: three `is_empty`-class probes.
    pub(crate) fn end_tick(&mut self) {
        if !self.incoming.is_empty() {
            for slot in std::mem::take(&mut self.incoming) {
                if let Some(o) = self.open.remove(&slot.open) {
                    self.counts.requests_untaken += 1;
                    self.live_remove(o.conn, o.wire_id);
                    self.reject(o.conn, o.wire_id, slot.op, rpc::UNTAKEN_REASON);
                }
            }
        }
        if !self.open.is_empty() {
            for (_, o) in std::mem::take(&mut self.open) {
                self.counts.requests_unanswered += 1;
                self.live_remove(o.conn, o.wire_id);
                self.reject(o.conn, o.wire_id, o.op, rpc::UNANSWERED_REASON);
            }
        }
        if self.waiting > 0 {
            for b in self.buckets.values_mut() {
                let n = b.len();
                if n > 0 {
                    self.counts.responses_unhandled += n as u64;
                    b.clear();
                }
            }
            self.waiting = 0;
        }
    }

    /// The client answers produced since the last call (engine answers,
    /// replies, `.answers` and External reports), in order. Empty and
    /// allocation-free when there are none.
    pub(crate) fn take_outbox(&mut self) -> Vec<(ConnectionId, RpcReply)> {
        if self.outbox.is_empty() {
            return Vec::new();
        }
        std::mem::take(&mut self.outbox)
    }

    // ---- session ends ----------------------------------------------------------------

    /// A session ended (leave, detach, a superseding join, migration out):
    /// its explicit binding goes, and every answer it was owed is
    /// abandoned — its open requests, its External requests in flight
    /// (dropped: a late report is `requests_late`), and the `.answers` of
    /// calls still in flight (the call itself goes on for its owner) —
    /// each counted once as `requests_abandoned`.
    pub(crate) fn drop_session(&mut self, conn: ConnectionId, m: &mut RoomCounters) {
        self.bound.remove(&conn);
        self.park_session(conn, m);
    }

    /// A session parked (detach-hold): what it was owed is abandoned as
    /// [`Self::drop_session`] abandons it, but its explicit binding
    /// (`calls.bind`) stays with the parked row — a resume moves it onto
    /// the new session ([`Self::resume_session`], BACKLOG B156); the
    /// hold's end drops it with the row.
    pub(crate) fn park_session(&mut self, conn: ConnectionId, m: &mut RoomCounters) {
        if self.owed(conn) == 0 && self.incoming.is_empty() {
            return;
        }
        let mut abandoned = 0u64;
        // Requests of this tick not taken yet, and taken ones not answered.
        if !self.incoming.is_empty() {
            let open = &mut self.open;
            self.incoming.retain(|s| {
                if s.conn == conn {
                    open.remove(&s.open);
                    abandoned += 1;
                    false
                } else {
                    true
                }
            });
        }
        let before = self.open.len();
        self.open.retain(|_, o| o.conn != conn);
        abandoned += (before - self.open.len()) as u64;
        // External requests in flight for it: dropped.
        let legacy: Vec<CallKey> = self
            .pending
            .iter()
            .filter(|(_, e)| e.legacy && e.answer.as_ref().is_some_and(|a| a.conn == conn))
            .map(|(k, _)| *k)
            .collect();
        for k in legacy {
            if let Some(e) = self.pending.remove(&k) {
                self.untrack(&e);
                abandoned += 1;
            }
        }
        // Calls that would answer it: the call goes on, the answer does
        // not.
        for e in self.pending.values_mut().chain(self.drafts.iter_mut()) {
            if e.answer.as_ref().is_some_and(|a| a.conn == conn) {
                e.answer = None;
                abandoned += 1;
            }
        }
        m.requests_abandoned += abandoned;
        self.live.retain(|(c, _)| *c != conn);
        self.owed.remove(&conn);
    }

    /// A parked session resumed through the fresh transport `new`: its
    /// explicit binding moves with it (BACKLOG B156). Nothing else of the
    /// old session survives the park.
    pub(crate) fn resume_session(&mut self, old: ConnectionId, new: ConnectionId) {
        if let Some(owner) = self.bound.remove(&old) {
            self.bound.insert(new, owner);
        }
    }

    /// The actor stops: every answer still owed is abandoned (counted as
    /// a session end counts it); nothing is in flight afterwards.
    pub(crate) fn drop_all_at_stop(&mut self, m: &mut RoomCounters) {
        let conns: Vec<ConnectionId> = self.owed.keys().copied().collect();
        for c in conns {
            self.drop_session(c, m);
        }
        let legacy: Vec<CallKey> = self
            .pending
            .iter()
            .filter(|(_, e)| e.legacy)
            .map(|(k, _)| *k)
            .collect();
        for k in legacy {
            if let Some(e) = self.pending.remove(&k) {
                self.untrack(&e);
            }
        }
        self.legacy_total = 0;
        self.legacy_by_conn.clear();
    }

    /// Capacities of the per-tick buffers (the idle-cost proof: all stay
    /// zero when nothing happens).
    pub fn buffer_capacities(&self) -> [usize; 5] {
        [
            self.incoming.capacity(),
            self.drafts.capacity(),
            self.local.capacity(),
            self.outbox.capacity(),
            self.buckets.capacity(),
        ]
    }
}
