//! The `Calls` table (SERVICES.md §5): ECS-agnostic, one per tick actor.
//!
//! - client requests come IN through it (`take_incoming`,
//!   `take_incoming_all`) and are answered through it (`reply`, or a
//!   call's `.answers(..)`);
//! - calls go OUT through it (`call(..)`), never blocking, never failing
//!   on the spot;
//! - results come BACK through it (`take_resolved`), one path for every
//!   ending (answer, service error, refusal, timeout, abandonment).
//!
//! What nobody takes or answers is closed at tick end and counted.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::marker::PhantomData;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::contract::{
    ActorId, Call, CallCx, CallError, CallKey, Caller, ConnId, DecodeError, Ended, Erased, Failure,
    Identity, Outcome, Owner, Refusal, Request, WireMessage, clone_outcome, ended_of, fail_for,
};
use crate::route::{
    CallSpec, Completion, ReplySink, SendFail, SendFn, ServiceId, ServiceStats, SinkTo,
};
use crate::wire::{RpcRequest, RpcResponse, reason};

/// One client request of type `R`, as the game takes it.
///
/// The fields are public so a game can move parts of `body` into a call
/// and still hand `reply` to `.answers(..)` (partial moves).
pub struct Incoming<R> {
    pub body: R,
    /// The verified identity of the sender (from the ticket, never from
    /// the payload).
    pub from: Identity,
    pub conn: ConnId,
    pub reply: ReplyTo<R>,
}

/// The right to answer one request, typed by it. Not `Clone`: at most one
/// answer. Must be used (`calls.reply`) or attached (`.answers`) in the
/// tick it was taken; otherwise the engine closes the request at tick
/// end (`requests_unanswered`).
pub struct ReplyTo<R> {
    id: u64,
    _r: PhantomData<fn() -> R>,
}

impl<R> std::fmt::Debug for ReplyTo<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyTo").field("id", &self.id).finish()
    }
}

/// A request of any type, from `take_incoming_all` (canonical order).
pub struct AnyIncoming {
    op: u16,
    item: Box<dyn Any + Send + Sync>,
}

impl AnyIncoming {
    pub fn op(&self) -> u16 {
        self.op
    }
    /// `Ok` if this is an `R`; the request back otherwise (chainable).
    pub fn downcast<R: Request>(self) -> Result<Incoming<R>, AnyIncoming> {
        match self.item.downcast::<Incoming<R>>() {
            Ok(b) => Ok(*b),
            Err(item) => Err(AnyIncoming { op: self.op, item }),
        }
    }
}

/// One resolved call: whom it belonged to, the context the caller put
/// on it, and how it ended.
pub struct Resolved<C: Call, X = ()> {
    pub key: CallKey,
    pub owner: Owner,
    pub ctx: X,
    pub outcome: Outcome<C>,
}

/// One recorded response: the key, the call type it answered (to catch
/// a diverging replay), and the outcome (`None`: it arrived late).
pub struct Recorded {
    pub key: CallKey,
    pub call: &'static str,
    pub outcome: Option<Erased>,
}

/// Boot-time registration errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// Two request types claim one opcode.
    OpCollision {
        op: u16,
        first: &'static str,
        second: &'static str,
    },
}

/// Every count has one meaning ("count every loss").
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    // Issue side.
    pub calls_issued: u64,
    pub calls_sent: u64,
    pub calls_refused_full: u64,
    pub calls_refused_gone: u64,
    pub calls_refused_share: u64,
    pub calls_exported: u64,
    pub calls_imported: u64,
    // How calls ended (each issued call ends in exactly one, or is exported).
    pub results_ok: u64,
    pub results_service_error: u64,
    pub results_refused: u64,
    pub results_timed_out: u64,
    pub results_abandoned: u64,
    /// An answer for a key this table no longer holds (exported away).
    pub results_late: u64,
    // Where results went.
    pub responses_delivered: u64,
    pub responses_unhandled: u64,
    pub responses_owner_gone: u64,
    pub answers_sent: u64,
    // Client requests.
    pub requests_received: u64,
    pub requests_malformed: u64,
    pub requests_no_handler: u64,
    pub requests_undecodable: u64,
    pub requests_duplicate: u64,
    pub requests_forwarded: u64,
    pub requests_answered: u64,
    pub requests_untaken: u64,
    pub requests_unanswered: u64,
    pub requests_replied_late: u64,
    // Replay.
    /// A recorded response met a different call than it answered: the
    /// game (or binary) is not the recorded one. Never in a true replay.
    pub replay_diverged: u64,
}

/// Table knobs.
#[derive(Debug, Clone, Copy)]
pub struct CallsConfig {
    pub actor: ActorId,
    /// High half of every key (rooms: derived from the recorded seed).
    pub origin: u64,
    /// In-flight cap per (this actor, service): the actor's share.
    pub share: u32,
    /// Completion channel capacity.
    pub completions: usize,
}

impl Default for CallsConfig {
    fn default() -> Self {
        Self {
            actor: ActorId(1),
            origin: 1,
            share: 64,
            completions: 1024,
        }
    }
}

type Decoder = fn(&[u8], ConnId, Identity, u64) -> Result<Box<dyn Any + Send + Sync>, DecodeError>;
type ForwardFn =
    Arc<dyn Fn(&mut Calls, ConnId, &Identity, u64, &[u8]) -> Result<(), DecodeError> + Send + Sync>;
type DeliverFn = fn(&mut Buckets, CallKey, Owner, Box<dyn Any + Send + Sync>, Erased);
type Buckets = HashMap<(TypeId, TypeId), Box<dyn Bucket>>;

trait Bucket: Send + Sync {
    fn len(&self) -> usize;
    fn clear(&mut self);
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Send + Sync + 'static> Bucket for Vec<T> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn clear(&mut self) {
        Vec::clear(self)
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

struct Slot {
    type_id: TypeId,
    op: u16,
    open: u64,
    item: Box<dyn Any + Send + Sync>,
}

struct OpenReq {
    conn: ConnId,
    wire_id: u64,
    from: Identity,
}

struct Answer {
    conn: ConnId,
    wire_id: u64,
    encode: fn(&Erased, u64) -> RpcResponse,
}

/// A call as the table keeps it (before and after it is sent). Also what
/// an export carries.
struct Entry {
    key: Option<CallKey>,
    type_id: TypeId,
    type_name: &'static str,
    owner: Owner,
    service: ServiceId,
    stats: Arc<ServiceStats>,
    send: SendFn,
    player: Option<Identity>,
    /// `None`: the game does not want the result (a pure `.answers`).
    ctx: Option<Box<dyn Any + Send + Sync>>,
    deliver: DeliverFn,
    answer: Option<Answer>,
    fail: fn(Failure) -> Erased,
    ended: fn(&Erased) -> Ended,
    clone: fn(&Erased) -> Erased,
    sent: bool,
}

/// Calls carried out of an actor by `export` (migration). In-process only
/// in this spike: the context is a `Box<dyn Any>` (see SERVICES.md open
/// questions for the cross-process form).
pub struct Exported {
    calls: Vec<Entry>,
}

impl Exported {
    pub fn len(&self) -> usize {
        self.calls.len()
    }
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
    /// The keys that travel (the destination re-issues under them).
    pub fn keys(&self) -> Vec<Option<CallKey>> {
        self.calls.iter().map(|e| e.key).collect()
    }
}

/// The per-actor call table. See the module docs.
///
/// A bevy `Resource` (in the engine: behind a `bevy` feature of the core
/// crate — the adapter crate could not implement bevy's trait for a gsb
/// type, the orphan rule).
#[derive(bevy_ecs::prelude::Resource)]
pub struct Calls {
    cfg: CallsConfig,
    seq: u64,
    replay: bool,
    // Client requests.
    ops: HashMap<u16, (TypeId, &'static str)>,
    accepts: HashMap<u16, Decoder>,
    forwards: HashMap<u16, ForwardFn>,
    incoming: Vec<Slot>,
    open: BTreeMap<u64, OpenReq>,
    live_ids: BTreeSet<(ConnId, u64)>,
    next_open: u64,
    // Calls.
    drafts: Vec<Entry>,
    pending: BTreeMap<CallKey, Entry>,
    in_flight_by: HashMap<(TypeId, Owner), u32>,
    per_service: HashMap<ServiceId, u32>,
    tx: mpsc::Sender<Completion>,
    rx: mpsc::Receiver<Completion>,
    local: Vec<Completion>,
    buckets: Buckets,
    waiting: usize,
    // Out.
    outbox: Vec<(ConnId, RpcResponse)>,
    recording: bool,
    recorded: Vec<Recorded>,
    pub counters: Counters,
    /// Completion-channel polls (one empty `try_recv` per idle tick).
    pub polls: u64,
}

impl Calls {
    pub fn new(cfg: CallsConfig) -> Self {
        let (tx, rx) = mpsc::channel(cfg.completions.max(1));
        Self {
            cfg,
            seq: 0,
            replay: false,
            ops: HashMap::new(),
            accepts: HashMap::new(),
            forwards: HashMap::new(),
            incoming: Vec::new(),
            open: BTreeMap::new(),
            live_ids: BTreeSet::new(),
            next_open: 0,
            drafts: Vec::new(),
            pending: BTreeMap::new(),
            in_flight_by: HashMap::new(),
            per_service: HashMap::new(),
            tx,
            rx,
            local: Vec::new(),
            buckets: HashMap::new(),
            waiting: 0,
            outbox: Vec::new(),
            recording: false,
            recorded: Vec::new(),
            counters: Counters::default(),
            polls: 0,
        }
    }

    pub fn config(&self) -> &CallsConfig {
        &self.cfg
    }

    // ---- boot ------------------------------------------------------------

    fn claim_op<R: Request>(&mut self) -> Result<(), BootError> {
        let name = std::any::type_name::<R>();
        if let Some((_, first)) = self.ops.get(&R::OP) {
            return Err(BootError::OpCollision {
                op: R::OP,
                first,
                second: name,
            });
        }
        self.ops.insert(R::OP, (TypeId::of::<R>(), name));
        Ok(())
    }

    /// The game takes `R` itself (`take_incoming::<R>()`).
    pub fn accept<R: Request>(&mut self) -> Result<(), BootError> {
        self.claim_op::<R>()?;
        self.accepts.insert(R::OP, decode_incoming::<R>);
        Ok(())
    }

    /// A shared one-line rule: every incoming `R` becomes the call `route`
    /// builds, answered straight to the client. The game never sees it.
    ///
    /// ```ignore
    /// calls.forward(move |r: ListFriends| friends.list(r))?;
    /// ```
    pub fn forward<R, C, F>(&mut self, route: F) -> Result<(), BootError>
    where
        R: Request,
        C: Call<Response = R::Response, Error = R::Error>,
        F: Fn(R) -> CallSpec<C> + Send + Sync + 'static,
    {
        self.claim_op::<R>()?;
        let rule: ForwardFn = Arc::new(move |calls, conn, from, wire_id, bytes| {
            let body = R::from_wire(bytes)?;
            let spec = route(body);
            let mut e = entry_of::<C, ()>(spec, None, None, Some(from.clone()), None);
            e.answer = Some(Answer {
                conn,
                wire_id,
                encode: encode_answer::<R>,
            });
            calls.push(e);
            Ok(())
        });
        self.forwards.insert(R::OP, rule);
        Ok(())
    }

    // ---- requests ----------------------------------------------------------

    /// This tick's requests of type `R`, in arrival order (per-type FIFO).
    pub fn take_incoming<R: Request>(&mut self) -> Vec<Incoming<R>> {
        if self.incoming.is_empty() {
            return Vec::new();
        }
        let want = TypeId::of::<R>();
        let mut out = Vec::new();
        let mut rest = Vec::with_capacity(self.incoming.len());
        for slot in self.incoming.drain(..) {
            if slot.type_id == want {
                match slot.item.downcast::<Incoming<R>>() {
                    Ok(b) => out.push(*b),
                    Err(_) => unreachable!("slot type id matches"),
                }
            } else {
                rest.push(slot);
            }
        }
        self.incoming = rest;
        out
    }

    /// All of this tick's requests, in the tick's canonical arrival order
    /// (the order READ pulled them, the order the input log records).
    pub fn take_incoming_all(&mut self) -> Vec<AnyIncoming> {
        self.incoming
            .drain(..)
            .map(|s| AnyIncoming {
                op: s.op,
                item: s.item,
            })
            .collect()
    }

    /// Answer a request now (typed).
    pub fn reply<R: Request>(&mut self, to: ReplyTo<R>, result: Result<R::Response, R::Error>) {
        match self.open.remove(&to.id) {
            Some(o) => {
                let out: Outcome<R> = result.map_err(CallError::Service);
                let erased: Erased = Box::new(out);
                self.outbox
                    .push((o.conn, encode_answer::<R>(&erased, o.wire_id)));
                self.live_ids.remove(&(o.conn, o.wire_id));
                self.counters.requests_answered += 1;
            }
            None => self.counters.requests_replied_late += 1,
        }
    }

    // ---- calls ---------------------------------------------------------------

    /// Issue a call. Never blocks, never fails here: refusals come back
    /// through `take_resolved` like every other ending.
    pub fn call<C: Call>(&mut self, spec: CallSpec<C>) -> CallBuilder<'_, C, ()> {
        CallBuilder {
            calls: Some(self),
            b: Some(Build {
                spec,
                owner: None,
                ctx: (),
                has_ctx: false,
                answer: None,
                player: None,
                key: None,
            }),
        }
    }

    /// This tick's (and earlier ticks' untaken-so-far) results of `C`
    /// issued with context type `X`, in arrival order.
    pub fn take_resolved<C: Call, X: Send + Sync + 'static>(&mut self) -> Vec<Resolved<C, X>> {
        if self.waiting == 0 {
            return Vec::new();
        }
        match self
            .buckets
            .get_mut(&(TypeId::of::<C>(), TypeId::of::<X>()))
        {
            Some(b) => {
                let v = b
                    .as_any_mut()
                    .downcast_mut::<Vec<Resolved<C, X>>>()
                    .expect("bucket type matches its key");
                let v = std::mem::take(v);
                self.waiting -= v.len();
                v
            }
            None => Vec::new(),
        }
    }

    /// Is a call of type `C` owned by `owner` in flight (or about to be)?
    pub fn in_flight<C: Call>(&self, owner: Owner) -> bool {
        self.in_flight_by
            .get(&(TypeId::of::<C>(), owner))
            .is_some_and(|n| *n > 0)
    }

    /// Carry `owner`'s calls out (migration). Their keys and contexts
    /// travel; a late answer here is `results_late`.
    pub fn export(&mut self, owner: Owner) -> Exported {
        let keys: Vec<CallKey> = self
            .pending
            .iter()
            .filter(|(_, e)| e.owner == owner)
            .map(|(k, _)| *k)
            .collect();
        let mut calls = Vec::with_capacity(keys.len());
        for k in keys {
            if let Some(e) = self.pending.remove(&k) {
                self.untrack(&e);
                calls.push(e);
            }
        }
        let mut i = 0;
        while i < self.drafts.len() {
            if self.drafts[i].owner == owner {
                let e = self.drafts.remove(i);
                self.untrack(&e);
                calls.push(e);
            } else {
                i += 1;
            }
        }
        self.counters.calls_exported += calls.len() as u64;
        Exported { calls }
    }

    /// Take calls exported elsewhere, now owned by `owner`; each is
    /// re-issued at this tick's flush under its ORIGINAL key (the service
    /// dedupes by it: one effect).
    pub fn import(&mut self, exported: Exported, owner: Owner) {
        for mut e in exported.calls {
            e.owner = owner;
            e.sent = false;
            self.counters.calls_imported += 1;
            self.push(e);
        }
    }

    fn untrack(&mut self, e: &Entry) {
        if let Some(n) = self.in_flight_by.get_mut(&(e.type_id, e.owner)) {
            *n -= 1;
            if *n == 0 {
                self.in_flight_by.remove(&(e.type_id, e.owner));
            }
        }
        if e.sent
            && let Some(n) = self.per_service.get_mut(&e.service)
        {
            *n -= 1;
        }
    }

    fn push(&mut self, e: Entry) {
        *self.in_flight_by.entry((e.type_id, e.owner)).or_insert(0) += 1;
        self.counters.calls_issued += 1;
        self.drafts.push(e);
    }

    // ---- engine phases (the actor's drain points) ------------------------------

    pub(crate) fn set_replay(&mut self, on: bool) {
        self.replay = on;
    }

    pub(crate) fn set_recording(&mut self, on: bool) {
        self.recording = on;
    }

    pub(crate) fn take_recorded(&mut self) -> Vec<Recorded> {
        std::mem::take(&mut self.recorded)
    }

    /// RESPONSES: absorb finished calls (refusals decided at the last
    /// flush first, then the completion channel in arrival order — or, in
    /// replay, the recorded list). `alive` says whether an owner still
    /// exists.
    pub(crate) fn responses(&mut self, replay: Option<&[Recorded]>, alive: &dyn Fn(Owner) -> bool) {
        if let Some(rec) = replay {
            for r in rec {
                let outcome = match (&r.outcome, self.pending.get(&r.key)) {
                    (Some(o), Some(e)) if e.type_name == r.call => (e.clone)(o),
                    (Some(_), Some(_)) => {
                        self.counters.replay_diverged += 1;
                        continue;
                    }
                    _ => {
                        self.counters.results_late += 1;
                        continue;
                    }
                };
                self.absorb(r.key, outcome, alive);
            }
            return;
        }
        if !self.local.is_empty() {
            for c in std::mem::take(&mut self.local) {
                self.absorb(c.key, c.outcome, alive);
            }
        }
        self.polls += 1;
        while let Ok(c) = self.rx.try_recv() {
            self.absorb(c.key, c.outcome, alive);
            self.polls += 1;
        }
    }

    fn absorb(&mut self, key: CallKey, outcome: Erased, alive: &dyn Fn(Owner) -> bool) {
        let Some(e) = self.pending.remove(&key) else {
            self.counters.results_late += 1;
            if self.recording {
                self.recorded.push(Recorded {
                    key,
                    call: "",
                    outcome: None,
                });
            }
            return;
        };
        self.untrack(&e);
        if self.recording {
            self.recorded.push(Recorded {
                key,
                call: e.type_name,
                outcome: Some((e.clone)(&outcome)),
            });
        }
        match (e.ended)(&outcome) {
            Ended::Ok => self.counters.results_ok += 1,
            Ended::ServiceError => self.counters.results_service_error += 1,
            Ended::Refused(_) => self.counters.results_refused += 1,
            Ended::TimedOut => self.counters.results_timed_out += 1,
            Ended::Abandoned => self.counters.results_abandoned += 1,
        }
        if let Some(a) = &e.answer {
            self.outbox.push((a.conn, (a.encode)(&outcome, a.wire_id)));
            self.live_ids.remove(&(a.conn, a.wire_id));
            self.counters.answers_sent += 1;
        }
        if let Some(ctx) = e.ctx {
            if e.owner != Owner::ACTOR && !alive(e.owner) {
                self.counters.responses_owner_gone += 1;
            } else {
                (e.deliver)(&mut self.buckets, key, e.owner, ctx, outcome);
                self.waiting += 1;
                self.counters.responses_delivered += 1;
            }
        }
    }

    /// READ: one client request (already authenticated: `from` is the
    /// connection's verified identity).
    pub(crate) fn read(&mut self, conn: ConnId, from: &Identity, req: RpcRequest) {
        self.counters.requests_received += 1;
        if req.id == 0 {
            self.counters.requests_malformed += 1;
            self.reject(conn, req.id, req.op, reason::MALFORMED);
            return;
        }
        if self.live_ids.contains(&(conn, req.id)) {
            self.counters.requests_duplicate += 1;
            self.reject(conn, req.id, req.op, reason::DUPLICATE);
            return;
        }
        let Ok(op) = u16::try_from(req.op) else {
            self.counters.requests_no_handler += 1;
            self.reject(conn, req.id, req.op, reason::NO_HANDLER);
            return;
        };
        if let Some(rule) = self.forwards.get(&op).cloned() {
            match rule(self, conn, from, req.id, &req.payload) {
                Ok(()) => {
                    self.live_ids.insert((conn, req.id));
                    self.counters.requests_forwarded += 1;
                }
                Err(DecodeError) => {
                    self.counters.requests_undecodable += 1;
                    self.reject(conn, req.id, req.op, reason::UNDECODABLE);
                }
            }
            return;
        }
        let Some(decode) = self.accepts.get(&op).copied() else {
            self.counters.requests_no_handler += 1;
            self.reject(conn, req.id, req.op, reason::NO_HANDLER);
            return;
        };
        self.next_open += 1;
        let open = self.next_open;
        match decode(&req.payload, conn, from.clone(), open) {
            Ok(item) => {
                let type_id = self.ops[&op].0;
                self.open.insert(
                    open,
                    OpenReq {
                        conn,
                        wire_id: req.id,
                        from: from.clone(),
                    },
                );
                self.live_ids.insert((conn, req.id));
                self.incoming.push(Slot {
                    type_id,
                    op,
                    open,
                    item,
                });
            }
            Err(DecodeError) => {
                self.counters.requests_undecodable += 1;
                self.reject(conn, req.id, req.op, reason::UNDECODABLE);
            }
        }
    }

    fn reject(&mut self, conn: ConnId, id: u64, op: u32, why: &str) {
        self.outbox.push((conn, engine_reply(id, op, why)));
    }

    /// After SYSTEMS: send this tick's calls (in issue order). A refusal
    /// is decided here and delivered in the next RESPONSES, like any
    /// other ending.
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
        if self.replay {
            // Services are not called; recorded outcomes arrive by key.
            self.pending.insert(key, e);
            return;
        }
        let used = self.per_service.get(&e.service).copied().unwrap_or(0);
        if used >= self.cfg.share {
            self.counters.calls_refused_share += 1;
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
        };
        let sink = ReplySink {
            key,
            to: SinkTo::Actor(self.tx.clone()),
            fail: e.fail,
            stats: Arc::clone(&e.stats),
        };
        match (e.send)(cx, sink) {
            Ok(()) => {
                e.sent = true;
                *self.per_service.entry(e.service).or_insert(0) += 1;
                self.counters.calls_sent += 1;
            }
            Err((why, sink)) => {
                sink.disarm();
                let r = match why {
                    SendFail::Full => {
                        self.counters.calls_refused_full += 1;
                        Refusal::Full
                    }
                    SendFail::Gone => {
                        self.counters.calls_refused_gone += 1;
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

    /// BROADCAST: close what nobody took or answered (counted, and the
    /// client gets an engine answer), and hand out this tick's answers.
    pub(crate) fn end_tick(&mut self) -> Vec<(ConnId, RpcResponse)> {
        if !self.incoming.is_empty() {
            for slot in std::mem::take(&mut self.incoming) {
                if let Some(o) = self.open.remove(&slot.open) {
                    self.counters.requests_untaken += 1;
                    self.live_ids.remove(&(o.conn, o.wire_id));
                    self.outbox.push((
                        o.conn,
                        engine_reply(o.wire_id, u32::from(slot.op), reason::UNTAKEN),
                    ));
                }
            }
        }
        if !self.open.is_empty() {
            for (_, o) in std::mem::take(&mut self.open) {
                self.counters.requests_unanswered += 1;
                self.live_ids.remove(&(o.conn, o.wire_id));
                self.outbox
                    .push((o.conn, engine_reply(o.wire_id, 0, reason::UNANSWERED)));
            }
        }
        if self.waiting > 0 {
            for b in self.buckets.values_mut() {
                let n = b.len();
                if n > 0 {
                    self.counters.responses_unhandled += n as u64;
                    b.clear();
                }
            }
            self.waiting = 0;
        }
        std::mem::take(&mut self.outbox)
    }

    /// Calls this table still waits for.
    pub fn pending_len(&self) -> usize {
        self.pending.len() + self.drafts.len()
    }

    /// Capacities of the per-tick buffers (the idle-cost proof: all stay
    /// zero when nothing happens).
    pub fn buffer_capacities(&self) -> [usize; 6] {
        [
            self.incoming.capacity(),
            self.drafts.capacity(),
            self.local.capacity(),
            self.outbox.capacity(),
            self.recorded.capacity(),
            self.buckets.capacity(),
        ]
    }
}

/// The builder `calls.call(..)` returns. The call is issued when the
/// builder ends (end of the statement); every method is optional.
pub struct CallBuilder<'a, C: Call, X: Send + Sync + 'static = ()> {
    calls: Option<&'a mut Calls>,
    b: Option<Build<C, X>>,
}

struct Build<C: Call, X> {
    spec: CallSpec<C>,
    owner: Option<Owner>,
    ctx: X,
    has_ctx: bool,
    answer: Option<Answer>,
    player: Option<Identity>,
    key: Option<CallKey>,
}

impl<'a, C: Call, X: Send + Sync + 'static> CallBuilder<'a, C, X> {
    /// Whom the result belongs to: `in_flight::<C>(owner)`, export, and
    /// "owner gone" all use it.
    pub fn owner(mut self, owner: Owner) -> Self {
        if let Some(b) = &mut self.b {
            b.owner = Some(owner);
        }
        self
    }

    /// Context that comes back with the result (stays here; never sent).
    pub fn with<Y: Send + Sync + 'static>(mut self, ctx: Y) -> CallBuilder<'a, C, Y> {
        let calls = self.calls.take();
        let b = self.b.take().map(|b| Build {
            spec: b.spec,
            owner: b.owner,
            ctx,
            has_ctx: true,
            answer: b.answer,
            player: b.player,
            key: b.key,
        });
        CallBuilder { calls, b }
    }

    /// Forward the outcome to this client request automatically (typed:
    /// the request's answer and error must be the call's). Also makes the
    /// request's verified identity the call's `cx.player()`.
    pub fn answers<R>(mut self, to: ReplyTo<R>) -> Self
    where
        R: Request<Response = C::Response, Error = C::Error>,
    {
        if let (Some(calls), Some(b)) = (self.calls.as_deref_mut(), self.b.as_mut()) {
            match calls.open.remove(&to.id) {
                Some(o) => {
                    if b.player.is_none() {
                        b.player = Some(o.from);
                    }
                    b.answer = Some(Answer {
                        conn: o.conn,
                        wire_id: o.wire_id,
                        encode: encode_answer::<R>,
                    });
                }
                None => calls.counters.requests_replied_late += 1,
            }
        }
        self
    }

    /// The verified player the call acts for (`cx.player()`), when there
    /// is no request to `.answers`.
    pub fn on_behalf_of(mut self, who: &Identity) -> Self {
        if let Some(b) = &mut self.b {
            b.player = Some(who.clone());
        }
        self
    }
}

impl<C: Call, X: Send + Sync + 'static> Drop for CallBuilder<'_, C, X> {
    fn drop(&mut self) {
        if let (Some(calls), Some(b)) = (self.calls.take(), self.b.take()) {
            let wants = b.owner.is_some() || b.has_ctx || b.answer.is_none();
            let ctx = if wants { Some(b.ctx) } else { None };
            let mut e = entry_of::<C, X>(b.spec, b.owner, ctx, b.player, b.key);
            e.answer = b.answer;
            calls.push(e);
        }
    }
}

fn entry_of<C: Call, X: Send + Sync + 'static>(
    spec: CallSpec<C>,
    owner: Option<Owner>,
    ctx: Option<X>,
    player: Option<Identity>,
    key: Option<CallKey>,
) -> Entry {
    Entry {
        key,
        type_id: TypeId::of::<C>(),
        type_name: std::any::type_name::<C>(),
        owner: owner.unwrap_or(Owner::ACTOR),
        service: spec.service,
        stats: spec.stats,
        send: spec.send,
        player,
        ctx: ctx.map(|x| Box::new(x) as Box<dyn Any + Send + Sync>),
        deliver: deliver::<C, X>,
        answer: None,
        fail: fail_for::<C>,
        ended: ended_of::<C>,
        clone: clone_outcome::<C>,
        sent: false,
    }
}

fn deliver<C: Call, X: Send + Sync + 'static>(
    buckets: &mut Buckets,
    key: CallKey,
    owner: Owner,
    ctx: Box<dyn Any + Send + Sync>,
    outcome: Erased,
) {
    let ctx = *ctx.downcast::<X>().expect("context stored as its type");
    let outcome = *outcome
        .downcast::<Outcome<C>>()
        .expect("outcome stored as its call's type");
    let b = buckets
        .entry((TypeId::of::<C>(), TypeId::of::<X>()))
        .or_insert_with(|| Box::new(Vec::<Resolved<C, X>>::new()));
    b.as_any_mut()
        .downcast_mut::<Vec<Resolved<C, X>>>()
        .expect("bucket type matches its key")
        .push(Resolved {
            key,
            owner,
            ctx,
            outcome,
        });
}

fn decode_incoming<R: Request>(
    bytes: &[u8],
    conn: ConnId,
    from: Identity,
    open: u64,
) -> Result<Box<dyn Any + Send + Sync>, DecodeError> {
    let body = R::from_wire(bytes)?;
    Ok(Box::new(Incoming::<R> {
        body,
        from,
        conn,
        reply: ReplyTo {
            id: open,
            _r: PhantomData,
        },
    }))
}

fn engine_reply(id: u64, op: u32, why: &str) -> RpcResponse {
    RpcResponse {
        id,
        ok: false,
        op,
        reason: why.to_owned(),
        payload: Vec::new(),
        game_error: false,
    }
}

fn encode_answer<R: Request>(o: &Erased, wire_id: u64) -> RpcResponse {
    let op = u32::from(R::OP);
    match o.downcast_ref::<Outcome<R>>() {
        Some(Ok(resp)) => RpcResponse {
            id: wire_id,
            ok: true,
            op,
            reason: String::new(),
            payload: resp.to_wire(),
            game_error: false,
        },
        Some(Err(CallError::Service(e))) => RpcResponse {
            id: wire_id,
            ok: false,
            op,
            reason: String::new(),
            payload: e.to_wire(),
            game_error: true,
        },
        Some(Err(CallError::Refused(r))) => engine_reply(
            wire_id,
            op,
            match r {
                Refusal::Full => reason::REFUSED_FULL,
                Refusal::Gone => reason::REFUSED_GONE,
                Refusal::Share => reason::REFUSED_SHARE,
                Refusal::Shed => reason::REFUSED_SHED,
            },
        ),
        Some(Err(CallError::TimedOut)) => engine_reply(wire_id, op, reason::TIMED_OUT),
        Some(Err(CallError::Abandoned)) | None => engine_reply(wire_id, op, reason::ABANDONED),
    }
}
