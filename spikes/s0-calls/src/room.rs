//! A minimal synchronous room with the proposed phase order
//! (SERVICES.md §3):
//!
//! ```text
//! CONTROL → RESPONSES → READ → EVENTS → CONVERT (+requests) → SYSTEMS → BROADCAST
//! ```
//!
//! Not the engine's room: no world type, no groups, no snapshots — just
//! enough tick to prove the call/response/event paths and their costs.
//! Everything is pulled with `try_recv`; there is no await anywhere.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

use prost::Message;
use tokio::sync::mpsc;

use crate::calls::{CallConfig, CallKey, CallTable, Done, Outcome, Refused};
use crate::push::PushIn;
use crate::request::{ClientCall, ConnId, Rejection, Request, RequestTable, Via, encode_reply};
use crate::service::{Call, ServiceRef};
use crate::wire::{self, Frame, RpcResponse, op, reason};

/// A fire-and-forget client action (today's `Action`, minus the player).
#[derive(Debug, Clone)]
pub struct Action {
    pub conn: ConnId,
    pub op: u16,
    pub payload: Vec<u8>,
}

/// What a membership's inbound channel carries: the client's actions
/// (requests included, as today) and — proposed — `Dest::World` pushes,
/// diverted by READ like today's path marker.
#[derive(Debug)]
pub enum Inbound {
    Action(Action),
    Push(PushIn),
}

/// A hub request: an encoded `RpcRequest` plus the connection's own
/// outbound path (the requester is NOT a member of the hub).
#[derive(Debug)]
pub struct HubRequest {
    pub conn: ConnId,
    pub out: mpsc::Sender<Frame>,
    pub envelope: Vec<u8>,
}

/// The game logic. `Done` is the game's "a call finished" message type,
/// `Req` its decoded client-request type.
pub trait Logic {
    type Done: 'static;
    type Req: 'static;

    /// RESPONSES: this tick's finished calls, in arrival order (timeouts
    /// after answers). Called only when the batch is non-empty.
    fn on_done(&mut self, _ctx: &TickCtx<'_, Self::Done>, _done: &mut Vec<Done<Self::Done>>) {}

    /// EVENTS: this tick's `Dest::World` pushes. Only when non-empty.
    fn on_events(&mut self, _ctx: &TickCtx<'_, Self::Done>, _events: &mut Vec<PushIn>) {}

    /// CONVERT: fire-and-forget actions.
    fn ingest(&mut self, _ctx: &TickCtx<'_, Self::Done>, _actions: &mut Vec<Action>) {}

    /// CONVERT (after `ingest`): one decoded client request.
    fn on_request(&mut self, ctx: &TickCtx<'_, Self::Done>, req: Self::Req);

    /// SYSTEMS.
    fn update(&mut self, _ctx: &TickCtx<'_, Self::Done>) {}
}

struct OutReply {
    conn: ConnId,
    via: Via,
    resp: RpcResponse,
}

/// What the engine lends every hook: the tick, the call verb and the
/// reply verb. `D` is the logic's `Done` type, so a tag of the wrong type
/// does not compile.
pub struct TickCtx<'a, D> {
    pub tick: u64,
    calls: &'a CallTable<D>,
    replies: &'a Cell<Vec<OutReply>>,
}

impl<D: 'static> TickCtx<'_, D> {
    /// Call a service without waiting. `tag` builds the game's message
    /// from the outcome — an enum variant constructor works
    /// (`ShopDone::Bought`), or a closure carrying context. Fails fast:
    /// a full/gone mailbox or an exhausted share is refused here, counted,
    /// and handed back.
    pub fn call<R, M>(
        &self,
        svc: &ServiceRef<M>,
        req: R,
        tag: impl FnOnce(Outcome<R>) -> D + Send + 'static,
    ) -> Result<CallKey, Refused<R, D>>
    where
        R: Request,
        M: From<Call<R>>,
    {
        self.calls.call(svc, None, req, Box::new(tag))
    }

    /// [`Self::call`] under a key the game already holds (a re-issue
    /// after a migration or a restart: the service dedupes by it).
    pub fn call_keyed<R, M>(
        &self,
        svc: &ServiceRef<M>,
        key: CallKey,
        req: R,
        tag: impl FnOnce(Outcome<R>) -> D + Send + 'static,
    ) -> Result<CallKey, Refused<R, D>>
    where
        R: Request,
        M: From<Call<R>>,
    {
        self.calls.call(svc, Some(key), req, Box::new(tag))
    }

    /// Answer a client request (now, or later from a `Done`). Typed.
    pub fn reply<R: Request>(&self, call: ClientCall<R>, result: Result<R::Response, R::Error>) {
        let resp = encode_reply::<R>(call.id, result);
        let mut q = self.replies.take();
        q.push(OutReply {
            conn: call.conn,
            via: call.via,
            resp,
        });
        self.replies.set(q);
    }
}

/// The spike room's knobs.
#[derive(Debug, Clone, Copy)]
pub struct RoomConfig {
    /// This incarnation's origin (high half of every call key).
    pub origin: u64,
    pub calls: CallConfig,
    pub request_timeout_ticks: u64,
    pub max_pending_requests_per_conn: u32,
    pub max_actions_per_conn_per_tick: usize,
    pub max_hub_requests_per_tick: usize,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            origin: 1,
            calls: CallConfig::default(),
            request_timeout_ticks: 150,
            max_pending_requests_per_conn: 4,
            max_actions_per_conn_per_tick: 16,
            max_hub_requests_per_tick: 256,
        }
    }
}

/// Work the phases did — what the idle-cost proof reads.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PhaseStats {
    pub ticks: u64,
    /// RESPONSES phases run (each is: drain until the first empty
    /// `try_recv`).
    pub response_polls: u64,
    /// Deadline sweeps (run only while something is in flight).
    pub sweeps: u64,
    pub on_done_calls: u64,
    pub on_events_calls: u64,
}

/// Client-request counters (existing names where they exist today).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RoomCounters {
    pub requests_rejected_malformed: u64,
    pub requests_rejected_no_handler: u64,
    /// Proposed: the payload did not decode as the registered type.
    pub requests_rejected_undecodable: u64,
    pub requests_rejected_dup: u64,
    pub requests_rejected_conn_cap: u64,
    pub requests_timed_out: u64,
    /// Proposed: the logic answered a request the sweep already answered.
    pub requests_replied_late: u64,
    /// Proposed for the hub path: the connection's own path refused the
    /// answer (full or closed).
    pub hub_replies_undelivered: u64,
}

struct Member {
    rx: mpsc::Receiver<Inbound>,
    out: mpsc::Sender<Frame>,
}

struct ClientPending {
    due: u64,
    op: u16,
    via: Via,
}

struct RawReq {
    conn: ConnId,
    via: Via,
    env: wire::RpcRequest,
}

/// The spike room (or hub: a room with no members and a hub inbox).
pub struct Room<L: Logic> {
    cfg: RoomConfig,
    logic: L,
    tick: u64,
    calls: CallTable<L::Done>,
    requests: RequestTable<L::Req>,
    members: BTreeMap<ConnId, Member>,
    hub_rx: Option<mpsc::Receiver<HubRequest>>,
    client_pending: HashMap<(ConnId, u64), ClientPending>,
    pending_per_conn: HashMap<ConnId, u32>,
    replies: Cell<Vec<OutReply>>,
    done_buf: Vec<Done<L::Done>>,
    events_buf: Vec<PushIn>,
    pub stats: PhaseStats,
    pub counters: RoomCounters,
}

impl<L: Logic> Room<L> {
    pub fn new(cfg: RoomConfig, logic: L, requests: RequestTable<L::Req>) -> Self {
        let completion_capacity = 2 * cfg.calls.max_in_flight_per_service as usize * 4;
        Self {
            cfg,
            logic,
            tick: 0,
            calls: CallTable::new(cfg.origin, cfg.calls, completion_capacity),
            requests,
            members: BTreeMap::new(),
            hub_rx: None,
            client_pending: HashMap::new(),
            pending_per_conn: HashMap::new(),
            replies: Cell::new(Vec::new()),
            done_buf: Vec::new(),
            events_buf: Vec::new(),
            stats: PhaseStats::default(),
            counters: RoomCounters::default(),
        }
    }

    /// The tick clock (the test clock: it moves only when `tick` runs).
    pub fn now(&self) -> u64 {
        self.tick
    }

    pub fn logic(&self) -> &L {
        &self.logic
    }

    pub fn logic_mut(&mut self) -> &mut L {
        &mut self.logic
    }

    pub fn calls(&self) -> &CallTable<L::Done> {
        &self.calls
    }

    /// Internal buffer capacities (the idle proof: they never allocate
    /// while nothing arrives).
    pub fn buffer_capacities(&self) -> (usize, usize) {
        (self.done_buf.capacity(), self.events_buf.capacity())
    }

    /// CONTROL, modeled: a member joins; returns its inbound sender (what
    /// the connection actor holds) and its outbound receiver (the client).
    pub fn join(
        &mut self,
        conn: ConnId,
        capacity: usize,
    ) -> (mpsc::Sender<Inbound>, mpsc::Receiver<Frame>) {
        let (in_tx, in_rx) = mpsc::channel(capacity);
        let (out_tx, out_rx) = mpsc::channel(capacity);
        self.members.insert(
            conn,
            Member {
                rx: in_rx,
                out: out_tx,
            },
        );
        (in_tx, out_rx)
    }

    /// Make this room a hub: requests arrive from non-members.
    pub fn hub_inbox(&mut self, capacity: usize) -> mpsc::Sender<HubRequest> {
        let (tx, rx) = mpsc::channel(capacity);
        self.hub_rx = Some(rx);
        tx
    }

    /// Run the tick body once.
    pub fn tick(&mut self) {
        self.tick += 1;
        self.stats.ticks += 1;
        self.calls.set_now(self.tick);

        // -- CONTROL: joins/leaves are applied between ticks in the spike.

        // Answers to ship this tick (engine rejections + reconciled
        // logic answers). `Vec::new` does not allocate.
        let mut direct: Vec<OutReply> = Vec::new();

        // -- RESPONSES: absorb finished calls BEFORE reading new input.
        self.stats.response_polls += 1;
        if self.calls.absorb(&mut self.done_buf) {
            self.stats.sweeps += 1;
        }
        if !self.done_buf.is_empty() {
            self.stats.on_done_calls += 1;
            let ctx = TickCtx {
                tick: self.tick,
                calls: &self.calls,
                replies: &self.replies,
            };
            self.logic.on_done(&ctx, &mut self.done_buf);
            self.done_buf.clear();
            // Answers given here settle NOW, so this tick's READ already
            // sees those correlation ids as free.
            self.reconcile_replies(&mut direct);
        }

        // -- READ: pull each member's channel (bounded), divert requests
        //    and pushes; then the hub inbox.
        let mut actions: Vec<Action> = Vec::new();
        let mut raw: Vec<RawReq> = Vec::new();
        for (conn, m) in self.members.iter_mut() {
            let mut pulled = 0;
            while pulled < self.cfg.max_actions_per_conn_per_tick {
                match m.rx.try_recv() {
                    Ok(Inbound::Push(p)) => self.events_buf.push(p),
                    Ok(Inbound::Action(a)) => {
                        pulled += 1;
                        if a.op != op::RPC_REQ {
                            actions.push(a);
                            continue;
                        }
                        match wire::RpcRequest::decode(&a.payload[..]) {
                            Ok(env) if env.id != 0 => raw.push(RawReq {
                                conn: *conn,
                                via: Via::Room,
                                env,
                            }),
                            _ => {
                                self.counters.requests_rejected_malformed += 1;
                                direct.push(reject(*conn, Via::Room, 0, 0, reason::MALFORMED));
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        if let Some(hub) = &mut self.hub_rx {
            for _ in 0..self.cfg.max_hub_requests_per_tick {
                let Ok(h) = hub.try_recv() else { break };
                match wire::RpcRequest::decode(&h.envelope[..]) {
                    Ok(env) if env.id != 0 => raw.push(RawReq {
                        conn: h.conn,
                        via: Via::Hub(h.out),
                        env,
                    }),
                    _ => {
                        self.counters.requests_rejected_malformed += 1;
                        direct.push(reject(h.conn, Via::Hub(h.out), 0, 0, reason::MALFORMED));
                    }
                }
            }
        }

        let ctx = TickCtx {
            tick: self.tick,
            calls: &self.calls,
            replies: &self.replies,
        };

        // -- EVENTS: outside facts about our players, before their input.
        if !self.events_buf.is_empty() {
            self.stats.on_events_calls += 1;
            self.logic.on_events(&ctx, &mut self.events_buf);
            self.events_buf.clear();
        }

        // -- CONVERT: actions, then requests (today's order).
        if !actions.is_empty() {
            self.logic.ingest(&ctx, &mut actions);
        }
        for r in raw {
            let key = (r.conn, r.env.id);
            let op16 = r.env.op as u16;
            if self.client_pending.contains_key(&key) {
                self.counters.requests_rejected_dup += 1;
                direct.push(reject(r.conn, r.via, r.env.id, op16, reason::DUPLICATE));
                continue;
            }
            let owed = self.pending_per_conn.get(&r.conn).copied().unwrap_or(0);
            if owed >= self.cfg.max_pending_requests_per_conn {
                self.counters.requests_rejected_conn_cap += 1;
                direct.push(reject(r.conn, r.via, r.env.id, op16, reason::CONN_CAP));
                continue;
            }
            match self
                .requests
                .decode(r.conn, r.env.id, op16, r.via.clone(), &r.env.payload)
            {
                Ok(req) => {
                    self.client_pending.insert(
                        key,
                        ClientPending {
                            due: self.tick + self.cfg.request_timeout_ticks,
                            op: op16,
                            via: r.via,
                        },
                    );
                    self.pending_per_conn.insert(r.conn, owed + 1);
                    self.logic.on_request(&ctx, req);
                }
                Err(Rejection::NoHandler) => {
                    self.counters.requests_rejected_no_handler += 1;
                    direct.push(reject(r.conn, r.via, r.env.id, op16, reason::NO_HANDLER));
                }
                Err(Rejection::Undecodable) => {
                    self.counters.requests_rejected_undecodable += 1;
                    direct.push(reject(r.conn, r.via, r.env.id, op16, reason::UNDECODABLE));
                }
            }
        }

        // -- SYSTEMS.
        self.logic.update(&ctx);

        // -- BROADCAST: reconcile the logic's answers against the pending
        //    set, sweep client deadlines, ship.
        self.reconcile_replies(&mut direct);
        if !self.client_pending.is_empty() {
            let now = self.tick;
            let mut expired: Vec<(ConnId, u64)> = self
                .client_pending
                .iter()
                .filter(|(_, p)| p.due <= now)
                .map(|(k, _)| *k)
                .collect();
            expired.sort();
            for key in expired {
                let p = self.client_pending.remove(&key).expect("collected above");
                release(&mut self.pending_per_conn, key.0);
                self.counters.requests_timed_out += 1;
                direct.push(reject(key.0, p.via, key.1, p.op, reason::TIMEOUT));
            }
        }
        if direct.is_empty() {
            return;
        }
        let mut private: BTreeMap<ConnId, Vec<RpcResponse>> = BTreeMap::new();
        for out in direct {
            match out.via {
                Via::Room => private.entry(out.conn).or_default().push(out.resp),
                Via::Hub(tx) => {
                    if tx.try_send(Frame::RpcResp(out.resp)).is_err() {
                        self.counters.hub_replies_undelivered += 1;
                    }
                }
            }
        }
        for (conn, responses) in private {
            if let Some(m) = self.members.get(&conn) {
                // The engine re-queues a refused batch's answers (F14);
                // the spike's outbound channels are sized not to refuse.
                let _ = m.out.try_send(Frame::Private { responses });
            }
        }
    }
}

impl<L: Logic> Room<L> {
    /// Take the logic's answers: each settles its pending entry exactly
    /// once; an answer for an entry the sweep already answered is late.
    fn reconcile_replies(&mut self, direct: &mut Vec<OutReply>) {
        let answered = self.replies.take();
        for out in answered {
            let key = (out.conn, out.resp.id);
            if self.client_pending.remove(&key).is_none() {
                self.counters.requests_replied_late += 1;
                continue;
            }
            release(&mut self.pending_per_conn, out.conn);
            direct.push(out);
        }
    }
}

fn release(per_conn: &mut HashMap<ConnId, u32>, conn: ConnId) {
    if let Some(n) = per_conn.get_mut(&conn) {
        *n -= 1;
        if *n == 0 {
            per_conn.remove(&conn);
        }
    }
}

fn reject(conn: ConnId, via: Via, id: u64, op: u16, why: &str) -> OutReply {
    OutReply {
        conn,
        via,
        resp: RpcResponse {
            id,
            ok: false,
            op: u32::from(op),
            reason: why.to_string(),
            payload: Vec::new(),
            game_error: false,
        },
    }
}
