//! The [`Calls`] table: its game-facing API. The engine's drain points
//! (RESPONSES, READ, FLUSH, tick end) are in `engine.rs`; the call
//! builder in `builder.rs`.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::calls::builder::{Build, CallBuilder};
use crate::calls::contract::{
    ActorId, Call, CallError, CallKey, DecodeError, Ended, Erased, Failure, Identity, Outcome,
    Owner, Request,
};
use crate::calls::route::{CallSpec, Completion, SendFn, ServiceId, ServiceStats};
use crate::channel::{Inbox, Mailbox};
use crate::id::{ConnectionId, PlayerId};
use crate::metrics::CallCounts;
use crate::rpc::RpcReply;

/// One client request of type `R`, as the game takes it.
///
/// The fields are public so a game can move parts of `body` into a call
/// and still hand `reply` to `.answers(..)` (partial moves).
pub struct Incoming<R> {
    /// The decoded request.
    pub body: R,
    /// The owner the session is bound to (the engine's default: the
    /// player, `GameLogic::call_owner`; or the game's own
    /// `calls.bind(conn, owner)`). No lookup needed.
    pub owner: Owner,
    /// The session's stable player (the room's binding).
    pub player: PlayerId,
    /// The verified identity of the sender (from the ticket, never from
    /// the payload).
    pub from: Identity,
    /// The transport session the answer goes back to.
    pub conn: ConnectionId,
    /// The right to answer, once.
    pub reply: ReplyTo<R>,
}

/// The right to answer one request, typed by it. Not `Clone`: at most one
/// answer. Must be used (`calls.reply`) or attached (`.answers`) in the
/// tick it was taken; otherwise the engine closes the request at tick end
/// (`requests_unanswered`).
pub struct ReplyTo<R> {
    pub(crate) id: u64,
    pub(crate) _r: PhantomData<fn() -> R>,
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
    /// The request's op.
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

/// One resolved call: whom it belonged to, the context the caller put on
/// it, and how it ended.
pub struct Resolved<C: Call, X = ()> {
    /// The call's key.
    pub key: CallKey,
    /// The call's owner.
    pub owner: Owner,
    /// The context given with `.with(..)`.
    pub ctx: X,
    /// How the call ended.
    pub outcome: Outcome<C>,
}

/// Boot-time registration errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// Two request types claim one opcode (or one type is both accepted
    /// and forwarded).
    OpCollision {
        /// The opcode.
        op: u16,
        /// The type that claimed it first.
        first: &'static str,
        /// The type that claimed it second.
        second: &'static str,
    },
    /// A request type claims an op in the engine's band (`op < 1000`,
    /// `gsb_protocol::op::GAME_BAND_START`): the build refuses it for a
    /// `.proto` request; this catches a hand-written `impl Request`
    /// (B159).
    EngineOp {
        /// The opcode.
        op: u16,
        /// The type that claimed it.
        ty: &'static str,
    },
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::OpCollision { op, first, second } => {
                write!(
                    f,
                    "request op {op:#06x} claimed by both {first} and {second}"
                )
            }
            BootError::EngineOp { op, ty } => write!(
                f,
                "request op {op} of {ty} is in the engine band (< {})",
                gsb_protocol::op::GAME_BAND_START
            ),
        }
    }
}

impl std::error::Error for BootError {}

/// The default of [`CallsConfig::timeout`] (and `RoomConfig::call_timeout`):
/// twice the service host's default deadline
/// ([`DEFAULT_SERVICE_TIMEOUT`](crate::calls::DEFAULT_SERVICE_TIMEOUT)),
/// so a service's own timeout normally ends the call first.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Table knobs.
#[derive(Debug, Clone, Copy)]
pub struct CallsConfig {
    /// The calling actor (rooms: the room id).
    pub actor: ActorId,
    /// High half of every key.
    pub origin: u64,
    /// In-flight cap per (this table, service): the actor's share.
    pub share: u32,
    /// Completion channel capacity.
    pub completions: usize,
    /// The table's own deadline for a game call (SERVICES.md §19 S6,
    /// the backstop behind the service host's timeout): a call sent at
    /// FLUSH and still unanswered this long after ends `TimedOut` at a
    /// RESPONSES (`results_timed_out`; a later answer is
    /// `results_late`). `None`: no sweep.
    pub timeout: Option<Duration>,
}

impl Default for CallsConfig {
    fn default() -> Self {
        Self {
            actor: ActorId(1),
            origin: 1,
            share: 64,
            completions: 1024,
            timeout: Some(DEFAULT_CALL_TIMEOUT),
        }
    }
}

type Decoder = fn(
    &[u8],
    ConnectionId,
    PlayerId,
    Identity,
    Owner,
    u64,
) -> Result<Box<dyn Any + Send + Sync>, DecodeError>;
pub(crate) type ForwardFn = Arc<
    dyn Fn(&mut Calls, ConnectionId, &Identity, u64, &[u8]) -> Result<(), DecodeError>
        + Send
        + Sync,
>;
pub(crate) type DeliverFn = fn(&mut Buckets, CallKey, Owner, Box<dyn Any + Send + Sync>, Erased);
pub(crate) type Buckets = HashMap<(TypeId, TypeId), Box<dyn Bucket>>;

pub(crate) trait Bucket: Send + Sync {
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

/// One request this tick, waiting for the game to take it.
pub(crate) struct Slot {
    pub(crate) type_id: TypeId,
    pub(crate) op: u16,
    pub(crate) open: u64,
    pub(crate) conn: ConnectionId,
    pub(crate) item: Box<dyn Any + Send + Sync>,
}

/// A request owed an answer this tick (taken or not).
pub(crate) struct OpenReq {
    pub(crate) conn: ConnectionId,
    pub(crate) wire_id: u64,
    pub(crate) op: u16,
    pub(crate) from: Identity,
}

/// Where a call's outcome answers a client: the session, its correlation
/// id, the op echoed, and how the outcome becomes a reply.
pub(crate) struct Answer {
    pub(crate) conn: ConnectionId,
    pub(crate) wire_id: u64,
    pub(crate) op: u16,
    pub(crate) encode: Encode,
}

/// A call as the table keeps it (before and after it is sent). Also what
/// an export carries.
pub(crate) struct Entry {
    pub(crate) key: Option<CallKey>,
    pub(crate) type_id: TypeId,
    pub(crate) owner: Owner,
    pub(crate) service: ServiceId,
    pub(crate) stats: Arc<ServiceStats>,
    /// `None`: the built-in `External` path (sent at registration).
    pub(crate) send: Option<SendFn>,
    pub(crate) player: Option<Identity>,
    /// `None`: the game does not want the result (an answer-only call).
    pub(crate) ctx: Option<Box<dyn Any + Send + Sync>>,
    pub(crate) deliver: DeliverFn,
    pub(crate) answer: Option<Answer>,
    pub(crate) fail: fn(Failure) -> Erased,
    pub(crate) ended: fn(&Erased) -> Ended,
    pub(crate) sent: bool,
    /// The built-in path of `RequestDecision::External`: counted on the
    /// request counters (`requests_*`), never on the calls family.
    pub(crate) legacy: bool,
    /// The call's type name (a replay matches a recorded ending to it).
    #[cfg(feature = "record")]
    pub(crate) call: &'static str,
    /// Copies an outcome of the call's type (the input log's copy).
    #[cfg(feature = "record")]
    pub(crate) copy: fn(&Erased) -> Erased,
}

/// Calls carried out of a table by `export` (migration). In-process only
/// (the contexts are `Box<dyn Any>`; SERVICES.md §19 S8).
pub struct Exported {
    pub(crate) calls: Vec<Entry>,
    /// The sessions explicitly bound to the exported owner: rebound on
    /// import.
    pub(crate) conns: Vec<ConnectionId>,
}

impl Exported {
    /// How many calls travel.
    pub fn len(&self) -> usize {
        self.calls.len()
    }
    /// Nothing travels.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
    /// The keys that travel (the destination re-issues under them;
    /// `None` = issued but not flushed yet: the destination keys it).
    pub fn keys(&self) -> Vec<Option<CallKey>> {
        self.calls.iter().map(|e| e.key).collect()
    }
}

/// The per-actor call table (SERVICES.md §5). ECS-agnostic: a plain
/// struct. A room and a shard each own one and lend it to the game
/// ([`GameLogic::update_with_calls`](crate::room::GameLogic::update_with_calls),
/// [`GameLogic::boot_calls`](crate::room::GameLogic::boot_calls)).
///
/// - client requests come IN through it (`take_incoming`,
///   `take_incoming_all`) and are answered through it (`reply`, or a
///   call's `.answers(..)`);
/// - calls go OUT through it (`call(..)`), never blocking, never failing
///   on the spot;
/// - results come BACK through it (`take_resolved`), one path for every
///   ending (answer, service error, refusal, timeout, abandonment).
///
/// What nobody takes or answers is closed at tick end and counted
/// ([`CallCounts`]).
pub struct Calls {
    pub(crate) cfg: CallsConfig,
    pub(crate) seq: u64,
    // Client requests.
    pub(crate) ops: HashMap<u16, &'static str>,
    pub(crate) accepts: HashMap<u16, (TypeId, Decoder)>,
    pub(crate) forwards: HashMap<u16, ForwardFn>,
    pub(crate) incoming: Vec<Slot>,
    pub(crate) open: BTreeMap<u64, OpenReq>,
    /// Correlation ids owed an answer, per session (the duplicate gate of
    /// both request paths).
    pub(crate) live: HashSet<(ConnectionId, u64)>,
    pub(crate) owed: HashMap<ConnectionId, usize>,
    pub(crate) next_open: u64,
    pub(crate) bound: BTreeMap<ConnectionId, Owner>,
    // Calls.
    pub(crate) drafts: Vec<Entry>,
    pub(crate) pending: BTreeMap<CallKey, Entry>,
    pub(crate) in_flight_by: HashMap<(TypeId, Owner), u32>,
    pub(crate) per_service: HashMap<ServiceId, u32>,
    pub(crate) tx: Mailbox<Completion>,
    pub(crate) rx: Inbox<Completion>,
    pub(crate) local: Vec<Completion>,
    pub(crate) buckets: Buckets,
    pub(crate) waiting: usize,
    // The built-in `External` path.
    pub(crate) legacy_by_conn: HashMap<ConnectionId, usize>,
    pub(crate) legacy_total: usize,
    pub(crate) deadlines: VecDeque<(Instant, CallKey)>,
    /// Game calls' deadlines (`CallsConfig::timeout`), in send order =
    /// deadline order.
    pub(crate) call_deadlines: VecDeque<(Instant, CallKey)>,
    // Out.
    pub(crate) outbox: Vec<(ConnectionId, RpcReply)>,
    pub(crate) counts: CallCounts,
    // The input log and REPLAY (feature `record`, `record.rs`).
    #[cfg(feature = "record")]
    pub(crate) rec: Option<Box<crate::calls::record::Rec>>,
    #[cfg(feature = "record")]
    pub(crate) replay: bool,
    #[cfg(feature = "record")]
    pub(crate) codecs: crate::determinism::Codecs,
}

impl std::fmt::Debug for Calls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Calls")
            .field("cfg", &self.cfg)
            .field("pending", &self.pending.len())
            .field("counts", &self.counts)
            .finish_non_exhaustive()
    }
}

impl Default for Calls {
    fn default() -> Self {
        Self::new(CallsConfig::default())
    }
}

impl Calls {
    /// A table with `cfg`'s knobs.
    pub fn new(cfg: CallsConfig) -> Self {
        let (tx, rx) = crate::channel::channel(cfg.completions);
        Self {
            cfg,
            seq: 0,
            ops: HashMap::new(),
            accepts: HashMap::new(),
            forwards: HashMap::new(),
            incoming: Vec::new(),
            open: BTreeMap::new(),
            live: HashSet::new(),
            owed: HashMap::new(),
            next_open: 0,
            bound: BTreeMap::new(),
            drafts: Vec::new(),
            pending: BTreeMap::new(),
            in_flight_by: HashMap::new(),
            per_service: HashMap::new(),
            tx,
            rx,
            local: Vec::new(),
            buckets: HashMap::new(),
            waiting: 0,
            legacy_by_conn: HashMap::new(),
            legacy_total: 0,
            deadlines: VecDeque::new(),
            call_deadlines: VecDeque::new(),
            outbox: Vec::new(),
            counts: CallCounts::default(),
            #[cfg(feature = "record")]
            rec: None,
            #[cfg(feature = "record")]
            replay: false,
            #[cfg(feature = "record")]
            codecs: {
                let mut c = crate::determinism::Codecs::default();
                c.insert(
                    crate::calls::engine::EXTERNAL,
                    crate::calls::record::external_codec(),
                );
                c
            },
        }
    }

    /// The table's knobs.
    pub fn config(&self) -> &CallsConfig {
        &self.cfg
    }

    /// The table's counters (cumulative).
    pub fn counts(&self) -> &CallCounts {
        &self.counts
    }

    // ---- boot ------------------------------------------------------------

    fn claim_op<R: Request>(&mut self) -> Result<(), BootError> {
        let name = std::any::type_name::<R>();
        if R::OP < gsb_protocol::op::GAME_BAND_START {
            return Err(BootError::EngineOp {
                op: R::OP,
                ty: name,
            });
        }
        if let Some(first) = self.ops.get(&R::OP) {
            return Err(BootError::OpCollision {
                op: R::OP,
                first,
                second: name,
            });
        }
        self.ops.insert(R::OP, name);
        Ok(())
    }

    /// The game takes `R` itself (`take_incoming::<R>()`). An op the table
    /// claims never reaches `GameLogic::handle_request`.
    pub fn accept<R: Request>(&mut self) -> Result<(), BootError> {
        self.claim_op::<R>()?;
        self.accepts
            .insert(R::OP, (TypeId::of::<R>(), decode_incoming::<R>));
        Ok(())
    }

    /// A shared one-line rule: every incoming `R` becomes the call `route`
    /// builds, answered straight to the client. The game never sees it;
    /// it needs no owner (it works for an unbound session too).
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
            let mut e = entry_of::<C, ()>(spec, None, None, Some(from.clone()));
            e.answer = Some(Answer {
                conn,
                wire_id,
                op: R::OP,
                encode: encode_answer::<R>,
            });
            calls.push(e);
            Ok(())
        });
        self.forwards.insert(R::OP, rule);
        // The rule's call answers with a request's types: it has a wire
        // form, so a log file can carry its outcomes.
        #[cfg(feature = "record")]
        self.codecs.register::<C>();
        Ok(())
    }

    /// Whether any op is claimed (the READ fast path).
    pub(crate) fn claims_any(&self) -> bool {
        !self.ops.is_empty()
    }

    /// Whether `op` is claimed by `accept` or `forward`.
    pub fn handles(&self, op: u16) -> bool {
        self.ops.contains_key(&op)
    }

    // ---- sessions ----------------------------------------------------------

    /// Bind a session to an owner explicitly. The engine binds every
    /// session by default (`GameLogic::call_owner`, the player); an
    /// explicit bind overrides it for that session until the session
    /// ends (leave, detach, migration, superseding join).
    pub fn bind(&mut self, conn: ConnectionId, owner: Owner) {
        self.bound.insert(conn, owner);
    }

    /// Forget a session's explicit binding (the engine does this when the
    /// session ends).
    pub fn unbind(&mut self, conn: ConnectionId) {
        self.bound.remove(&conn);
    }

    /// The owner a session is explicitly bound to.
    pub fn bound(&self, conn: ConnectionId) -> Option<Owner> {
        self.bound.get(&conn).copied()
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
                    Err(_) => unreachable!("the slot's type id matches"),
                }
            } else {
                rest.push(slot);
            }
        }
        self.incoming = rest;
        out
    }

    /// All of this tick's requests, in the tick's canonical arrival order
    /// (the order READ pulled them).
    pub fn take_incoming_all(&mut self) -> Vec<AnyIncoming> {
        self.incoming
            .drain(..)
            .map(|s| AnyIncoming {
                op: s.op,
                item: s.item,
            })
            .collect()
    }

    /// Answer a request now (typed). A reply to a request the engine has
    /// already closed is ignored and counted (`requests_replied_late`).
    pub fn reply<R: Request>(&mut self, to: ReplyTo<R>, result: Result<R::Response, R::Error>) {
        match self.open.remove(&to.id) {
            Some(o) => {
                let out: Outcome<R> = result.map_err(CallError::Service);
                let erased: Erased = Box::new(out);
                self.answer_now(o.conn, o.wire_id, o.op, encode_answer::<R>, &erased);
                self.counts.requests_answered += 1;
            }
            None => self.counts.requests_replied_late += 1,
        }
    }

    // ---- calls ---------------------------------------------------------------

    /// Issue a call. Never blocks, never fails here: refusals come back
    /// through `take_resolved` like every other ending (decided at FLUSH,
    /// delivered at the next tick's RESPONSES). The call is written when
    /// the returned builder ends (the end of the statement).
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
            }),
        }
    }

    /// The results of `C` issued with context type `X`, in arrival order.
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

    /// Is a call of type `C` owned by `owner` in flight (or written this
    /// tick)?
    pub fn in_flight<C: Call>(&self, owner: Owner) -> bool {
        self.in_flight_by
            .get(&(TypeId::of::<C>(), owner))
            .is_some_and(|n| *n > 0)
    }

    /// Calls this table still waits for (written or in flight).
    pub fn pending_len(&self) -> usize {
        self.pending.len() + self.drafts.len()
    }

    /// Carry `owner`'s calls out (migration). Their keys and contexts
    /// travel; a late answer here is `results_late`.
    pub fn export(&mut self, owner: Owner) -> Exported {
        let keys: Vec<CallKey> = self
            .pending
            .iter()
            .filter(|(_, e)| e.owner == owner && !e.legacy)
            .map(|(k, _)| *k)
            .collect();
        let mut calls = Vec::with_capacity(keys.len());
        for k in keys {
            if let Some(e) = self.pending.remove(&k) {
                self.untrack(&e);
                if let Some(a) = &e.answer {
                    self.live_remove(a.conn, a.wire_id);
                }
                calls.push(e);
            }
        }
        let mut i = 0;
        while i < self.drafts.len() {
            if self.drafts[i].owner == owner {
                let e = self.drafts.remove(i);
                self.untrack(&e);
                if let Some(a) = &e.answer {
                    self.live_remove(a.conn, a.wire_id);
                }
                calls.push(e);
            } else {
                i += 1;
            }
        }
        self.counts.calls_exported += calls.len() as u64;
        let conns: Vec<ConnectionId> = self
            .bound
            .iter()
            .filter(|(_, o)| **o == owner)
            .map(|(c, _)| *c)
            .collect();
        for c in &conns {
            self.bound.remove(c);
        }
        Exported { calls, conns }
    }

    /// Take calls exported elsewhere, now owned by `owner`; each is
    /// re-issued at this tick's FLUSH under its ORIGINAL key (an
    /// idempotent service dedupes by it: one effect).
    pub fn import(&mut self, exported: Exported, owner: Owner) {
        for c in exported.conns {
            self.bound.insert(c, owner);
        }
        for mut e in exported.calls {
            e.owner = owner;
            e.sent = false;
            if let Some(a) = &e.answer {
                self.live_insert(a.conn, a.wire_id);
            }
            self.counts.calls_imported += 1;
            self.push(e);
        }
    }

    // ---- shared bookkeeping --------------------------------------------------

    pub(crate) fn untrack(&mut self, e: &Entry) {
        if let Some(n) = self.in_flight_by.get_mut(&(e.type_id, e.owner)) {
            *n -= 1;
            if *n == 0 {
                self.in_flight_by.remove(&(e.type_id, e.owner));
            }
        }
        if e.sent
            && !e.legacy
            && let Some(n) = self.per_service.get_mut(&e.service)
        {
            *n -= 1;
            if *n == 0 {
                self.per_service.remove(&e.service);
            }
        }
        if e.legacy
            && let Some(a) = &e.answer
        {
            self.legacy_total = self.legacy_total.saturating_sub(1);
            if let Some(n) = self.legacy_by_conn.get_mut(&a.conn) {
                *n -= 1;
                if *n == 0 {
                    self.legacy_by_conn.remove(&a.conn);
                }
            }
        }
    }

    pub(crate) fn push(&mut self, e: Entry) {
        *self.in_flight_by.entry((e.type_id, e.owner)).or_insert(0) += 1;
        self.counts.calls_issued += 1;
        self.drafts.push(e);
    }

    pub(crate) fn live_insert(&mut self, conn: ConnectionId, id: u64) {
        if self.live.insert((conn, id)) {
            *self.owed.entry(conn).or_insert(0) += 1;
        }
    }

    pub(crate) fn live_remove(&mut self, conn: ConnectionId, id: u64) {
        if self.live.remove(&(conn, id))
            && let Some(n) = self.owed.get_mut(&conn)
        {
            *n -= 1;
            if *n == 0 {
                self.owed.remove(&conn);
            }
        }
    }

    /// Queue a client answer now and free its correlation id.
    pub(crate) fn answer_now(
        &mut self,
        conn: ConnectionId,
        wire_id: u64,
        op: u16,
        encode: Encode,
        outcome: &Erased,
    ) {
        self.outbox.push((conn, encode(outcome, wire_id, op)));
        self.live_remove(conn, wire_id);
    }
}

pub(crate) fn entry_of<C: Call, X: Send + Sync + 'static>(
    spec: CallSpec<C>,
    owner: Option<Owner>,
    ctx: Option<X>,
    player: Option<Identity>,
) -> Entry {
    Entry {
        key: None,
        type_id: TypeId::of::<C>(),
        owner: owner.unwrap_or(Owner::ACTOR),
        service: spec.service,
        stats: spec.stats,
        send: Some(spec.send),
        player,
        ctx: ctx.map(|x| Box::new(x) as Box<dyn Any + Send + Sync>),
        deliver: deliver::<C, X>,
        answer: None,
        fail: crate::calls::contract::fail_for::<C>,
        ended: crate::calls::contract::ended_of::<C>,
        sent: false,
        legacy: false,
        #[cfg(feature = "record")]
        call: std::any::type_name::<C>(),
        #[cfg(feature = "record")]
        copy: crate::calls::contract::copy_outcome::<C>,
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
    conn: ConnectionId,
    player: PlayerId,
    from: Identity,
    owner: Owner,
    open: u64,
) -> Result<Box<dyn Any + Send + Sync>, DecodeError> {
    let body = R::from_wire(bytes)?;
    Ok(Box::new(Incoming::<R> {
        body,
        owner,
        player,
        from,
        conn,
        reply: ReplyTo {
            id: open,
            _r: PhantomData,
        },
    }))
}

/// How an outcome becomes the client's answer (`id`, `op` echoed).
pub(crate) type Encode = fn(&Erased, u64, u16) -> RpcReply;

/// A typed outcome as a client answer. A typed service error rides
/// `ok = false`, `game_error = true`, an EMPTY `reason` and the error in
/// `payload` (round q3, `docs/SERVICES.md` §16 — the flag is what tells a
/// default-valued error, which encodes to zero bytes, from an engine
/// answer); every engine answer has `game_error = false` and a non-empty
/// `reason`.
pub(crate) fn encode_answer<R: Request>(o: &Erased, id: u64, op: u16) -> RpcReply {
    use crate::calls::contract::{Refusal, WireMessage};
    use crate::rpc;
    let engine = |why: &str| RpcReply::rejected(id, op, why);
    match o.downcast_ref::<Outcome<R>>() {
        Some(Ok(resp)) => RpcReply::ok(id, op, Bytes::from(resp.to_wire())),
        Some(Err(CallError::Service(e))) => RpcReply::game_error(id, op, Bytes::from(e.to_wire())),
        Some(Err(CallError::Refused(r))) => engine(match r {
            Refusal::Full => rpc::REFUSED_FULL_REASON,
            Refusal::Gone => rpc::REFUSED_GONE_REASON,
            Refusal::Share => rpc::REFUSED_SHARE_REASON,
            Refusal::Shed => rpc::REFUSED_SHED_REASON,
        }),
        Some(Err(CallError::TimedOut)) => engine(rpc::TIMEOUT_REASON),
        Some(Err(CallError::Abandoned)) | None => engine(rpc::ABANDONED_REASON),
    }
}
