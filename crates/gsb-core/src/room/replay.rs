//! REPLAY (feature `record`, `docs/SERVICES.md` §10.2): run an input log
//! against the same game — the same logic, a fresh world — without
//! calling any service. Each tick's inputs are fed to the logic in the
//! recorded order (the CONTROL hooks, the recorded endings by key, the
//! table's reads, `ingest`, `handle_request`, SYSTEMS + FLUSH, the tick's
//! end) under the recorded `dt`, the room's seed and its clock; the
//! table, in replay mode, writes every call pending under its key and
//! never sends it. Each step's state hash is compared with the recorded
//! one: the first tick that differs is reported (the anti-cheat verify:
//! replay a match, compare).
//!
//! The EVENTS the log carries (`Input::Events`, `docs/EVENTS.md` §11)
//! enter a replay's own event table at their recorded place, with their
//! recorded owners — no router, no publisher. A recorded event the
//! replaying table does not admit (no `accept` for its op, a body that
//! does not decode) counts `replay_diverged` and reports its tick. A
//! room that publishes is built for its replay with a detached handle
//! ([`Publisher::detached`](crate::events::Publisher::detached)): its
//! pushes are output, not input — they go nowhere.
//!
//! The CONTROL answers the log carries — a join's admission, a hold's
//! `may_release` — are compared with the replay's own: a different one
//! counts `replay_diverged` and reports its tick like a hash would
//! (B175); the replay goes on with its own answer.
//!
//! No actor, no transport, no runtime: transport-made decisions
//! (congestion, a malformed envelope, a full room) never reached the
//! logic and are not in the log. The logic's input-idle and path views
//! read EMPTY in a replay (they are transport measurements, not
//! recorded): logic that branches on them does not replay.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::calls::Calls;
use crate::determinism::{Input, InputLog, RoomRng};
use crate::id::{ConnectionId, PlayerId};
use crate::events::{Addressee, Events};
use crate::metrics::{CallCounts, EventCounts};
use crate::room::{
    Action, IdleClock, IdleView, KickQueue, Parts, RoomConfig, RoomConn, RoomCounters, RoomLogic,
    TickCtx, call_table,
};
use crate::rpc::{RpcReply, RpcRequest};

/// A log replaying against a game.
pub struct Replay<W, G = (), Sp = ()> {
    log: Arc<InputLog>,
    next: usize,
    world: W,
    logic: Box<dyn RoomLogic<W, GroupKey = G, Strip = Sp>>,
    calls: Calls,
    events: Events,
    config: RoomConfig,
    m: RoomCounters,
    queued: HashMap<ConnectionId, Vec<RpcReply>>,
    conns: HashMap<PlayerId, RoomConn<G>>,
    rng: RoomRng,
    clock: Duration,
    hashes: Vec<Option<u64>>,
    first_divergent: Option<u64>,
}

/// One replayed step.
#[derive(Debug)]
pub struct ReplayTick {
    /// The recorded global tick.
    pub tick: u64,
    /// The replay's state hash.
    pub hash: Option<u64>,
    /// The recorded one.
    pub recorded: Option<u64>,
    /// The client answers the step produced, by session (each session's
    /// in order).
    pub answers: Vec<(ConnectionId, RpcReply)>,
}

/// A whole replay's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// Ticks replayed.
    pub ticks: u64,
    /// The replay's hashes, in order.
    pub hashes: Vec<Option<u64>>,
    /// The first recorded tick whose hash differed (`None`: none did).
    pub first_divergent: Option<u64>,
    /// The replaying table's counters (`replay_diverged`,
    /// `replay_undecodable` among them).
    pub counts: CallCounts,
    /// The replaying event table's counters.
    pub events: EventCounts,
}

impl<W, G, Sp> Replay<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// A replay of `log` over a fresh `world` and the game's `logic`
    /// (built as the recorded room's was; its `boot_calls` runs here).
    pub fn new(
        log: Arc<InputLog>,
        world: W,
        logic: Box<dyn RoomLogic<W, GroupKey = G, Strip = Sp>>,
    ) -> Self {
        let config = log.header.config();
        let seed = log.header.seed;
        let mut logic = logic;
        let (mut calls, _boot) = call_table(&config, config.id.0, seed, &mut *logic);
        calls.set_replay();
        let mut events = Events::sharing_ops(&calls);
        if let Err(e) = logic.boot_events(&mut events) {
            tracing::error!(error = %e, "replay: event table boot registration failed");
        }
        Self {
            log,
            next: 0,
            world,
            logic,
            calls,
            events,
            config,
            m: RoomCounters::default(),
            queued: HashMap::new(),
            conns: HashMap::new(),
            rng: RoomRng::new(seed),
            clock: Duration::ZERO,
            hashes: Vec::new(),
            first_divergent: None,
        }
    }

    /// Ticks left.
    pub fn left(&self) -> usize {
        self.log.ticks.len() - self.next
    }

    /// The replayed world.
    pub fn world(&self) -> &W {
        &self.world
    }

    /// The replaying table's counters.
    pub fn counts(&self) -> CallCounts {
        *self.calls.counts()
    }

    /// The replaying event table's counters.
    pub fn event_counts(&self) -> EventCounts {
        *self.events.counts()
    }

    /// The first recorded tick whose hash differed so far.
    pub fn first_divergent(&self) -> Option<u64> {
        self.first_divergent
    }

    /// Replay every tick left.
    pub fn run(&mut self) -> ReplayReport {
        while self.tick().is_some() {}
        ReplayReport {
            ticks: self.hashes.len() as u64,
            hashes: self.hashes.clone(),
            first_divergent: self.first_divergent,
            counts: self.counts(),
            events: self.event_counts(),
        }
    }

    /// Replay one tick (`None`: the log is done).
    pub fn tick(&mut self) -> Option<ReplayTick> {
        let log = Arc::clone(&self.log);
        let rec = log.ticks.get(self.next)?;
        self.next += 1;
        self.clock += rec.dt;
        let Self {
            world,
            logic,
            calls,
            events,
            config,
            m,
            queued,
            conns,
            rng,
            clock,
            ..
        } = self;
        let kicks = KickQueue::default();
        let idle = IdleClock::default();
        let paths = crate::path::PathTable::default();
        let ctx = TickCtx {
            room: config.id,
            tick: rec.tick,
            dt: rec.dt,
            time: *clock,
            rng: rng.view(),
            idle: IdleView::new(&idle, Instant::now()),
            kicks: kicks.kicks(),
            paths: paths.view(config.period()),
        };
        // A CONTROL answer that differs from the recorded one (B175).
        let mut diverged = 0u64;
        for input in &rec.inputs {
            let mut p = Parts {
                calls,
                m,
                queued,
                conns,
                config,
            };
            match input {
                Input::Join {
                    conn,
                    identity,
                    claims,
                    player,
                    entity,
                } => {
                    let joiner = crate::auth::Joiner::new(identity).with_claims(claims.as_ref());
                    let a = logic.on_join_verified(world, *conn, &joiner);
                    if (a.player, a.entity) != (*player, *entity) {
                        diverged += 1;
                    }
                }
                Input::Resume {
                    identity,
                    conn,
                    player,
                    entity,
                } => logic.on_resume(world, identity, *conn, *player, *entity),
                Input::Leave { player } => logic.on_leave(world, *player),
                Input::Disconnect {
                    player,
                    identity,
                    cause,
                } => {
                    logic.on_disconnect_with(world, *player, identity, *cause);
                }
                Input::Release { player, released } => {
                    if logic.may_release(world, *player) != *released {
                        diverged += 1;
                    }
                }
                Input::Expire { player, to } => logic.on_detach_expired(world, *player, *to),
                Input::BatchDropped { player, snapshot } => {
                    logic.on_batch_dropped(world, *player, *snapshot)
                }
                Input::BatchResumed { player } => logic.on_batch_resumed(world, *player),
                Input::SessionEnd { conn } => p.drop_session(*conn),
                Input::SessionPark { conn } => p.park_session(*conn),
                Input::SessionMove { from, to } => p.calls.resume_session(*from, *to),
                Input::Responses(list) => {
                    p.calls.replay_responses(list, p.m);
                    p.drain_outbox();
                }
                Input::Read {
                    conn,
                    player,
                    from,
                    owner,
                    wire_id,
                    op,
                    payload,
                } => {
                    p.calls
                        .read(*conn, *player, from, *owner, *wire_id, *op, payload);
                    p.drain_outbox();
                }
                Input::Events(list) => {
                    for e in list {
                        let to = match &e.to {
                            Some(id) => Addressee::Player(id.clone()),
                            None => Addressee::Room,
                        };
                        if !events.admit(e.op, &e.payload, to, e.owner, e.publisher, e.seq) {
                            diverged += 1;
                        }
                    }
                }
                Input::Actions(list) => {
                    let mut actions: Vec<Action> = list
                        .iter()
                        .map(|a| Action {
                            conn: a.conn,
                            player: a.player,
                            op: a.op,
                            payload: a.payload.clone(),
                        })
                        .collect();
                    logic.ingest(world, &ctx, &mut actions);
                }
                Input::Request {
                    conn,
                    player,
                    id,
                    op,
                    payload,
                } => {
                    let req = RpcRequest {
                        conn: *conn,
                        player: *player,
                        id: *id,
                        op: *op,
                        payload: payload.clone(),
                    };
                    p.handle(std::slice::from_ref(&req), &mut **logic, world, &ctx);
                }
                Input::Systems => {
                    logic.update_with_events(world, &ctx, p.calls, events);
                    p.calls.flush();
                }
                Input::EndTick => {
                    p.end_tick();
                    events.end_tick();
                }
            }
        }
        let mut answers: Vec<(ConnectionId, RpcReply)> = Vec::new();
        let mut by_conn: Vec<_> = queued.drain().collect();
        by_conn.sort_by_key(|(c, _)| *c);
        for (c, list) in by_conn {
            answers.extend(list.into_iter().map(|r| (c, r)));
        }
        calls.counts.replay_diverged += diverged;
        let game = logic.state_hash(world);
        let hash = crate::determinism::fold(game, rng.state(), *clock);
        self.hashes.push(hash);
        if (diverged > 0 || hash != rec.hash) && self.first_divergent.is_none() {
            self.first_divergent = Some(rec.tick);
        }
        Some(ReplayTick {
            tick: rec.tick,
            hash,
            recorded: rec.hash,
            answers,
        })
    }
}
