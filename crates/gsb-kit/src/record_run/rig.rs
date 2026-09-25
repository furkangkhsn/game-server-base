//! The twin: ONE live registry (one ticker, so the two rooms step in
//! lockstep) hosting two rooms of the same kind — room 1 runs the game
//! over the fixture's `entities` codec, room 2 over its record-run twin
//! — and a pair of clients per script player, one in each room, each
//! applying what its out channel received under the kit's client rules.
//! After every tick barrier the two views of every pair must be equal,
//! and so must every frame's content.
//!
//! The step barrier is the metrics channel (every room actor and shard
//! emits one sample per step after its broadcast phase); on the paused
//! clock a tick fires only when every task is idle, so a join, a leave
//! or an input sent between two barriers reaches both rooms on the same
//! tick.

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::channel::{FrameBatch, Inbox, Mailbox, channel};
use gsb_core::id::{ConnectionId, EntityId, PlayerId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{Action, RoomConfig};
use gsb_core::ticker::Ticker;
use tokio::sync::oneshot;

use super::compare::{Stats, add, same};
use super::game::{MOVE, SWITCH, to};
use super::layout::{Parts, take};
use super::script::Op;
use crate::client::{ClientView, Counters};
use crate::game::Game;
use crate::testing::{Dec, Fixture};

/// The `entities` room and the record-run room.
const ROOMS: [RoomId; 2] = [RoomId(1), RoomId(2)];
const WAIT: Duration = Duration::from_secs(30);
/// Keep-alive cadence, Hz (every 10th tick at 30 Hz).
const KEEPALIVE_HZ: f64 = 3.0;

type Joined = oneshot::Receiver<Result<(EntityId, Mailbox<Action>), gsb_core::error::CoreError>>;

/// One side of a pair: a session in one room.
struct Side<const RUN: bool> {
    conn: ConnectionId,
    out: Inbox<FrameBatch>,
    joined: Option<Joined>,
    actions: Option<Mailbox<Action>>,
    view: ClientView<Dec<RUN>>,
    /// This tick's frames, taken apart: `(private?, parts)`.
    frames: Vec<(bool, Parts)>,
}

impl<const RUN: bool> Side<RUN> {
    fn drain(&mut self, stats: &mut Stats) {
        self.frames.clear();
        while let Ok(batch) = self.out.try_recv() {
            for f in batch {
                let snapshot = f.op == <Fixture as Game>::SNAPSHOT_OP;
                take(&mut self.view, &mut self.frames, snapshot, &f.payload);
            }
        }
        stats.see(&self.frames);
    }

    fn send(&self, op: u16, payload: bytes::Bytes) {
        if let Some(actions) = &self.actions {
            actions
                .try_send(Action {
                    conn: self.conn,
                    player: PlayerId(0),
                    op,
                    payload,
                })
                .expect("action inbox has room");
        }
    }
}

/// The twin rooms and the client pairs.
pub(super) struct Rig {
    reg: Mailbox<RegistryMsg>,
    metrics: Inbox<MetricsEvent>,
    /// Shards per room (1: a single room actor).
    shards: u64,
    steps: HashMap<(u64, u64), u64>,
    pairs: Vec<(Side<false>, Side<true>)>,
    next_conn: u64,
    pub(super) tick: u64,
    /// What the `entities` side and the run side saw.
    pub(super) stats: (Stats, Stats),
}

impl Rig {
    /// A live registry over `factory` (room 1: `entities`, room 2: the
    /// run) with both rooms created.
    pub(super) async fn new<G, St, Sp>(factory: RoomFactory<World, G, St, Sp>, shards: u64) -> Self
    where
        G: Eq + std::hash::Hash + Clone + std::fmt::Debug + Send + 'static,
        St: std::fmt::Debug + Send + 'static,
        Sp: std::fmt::Debug + Clone + PartialEq + Send + 'static,
    {
        let (reg, rx) = channel::<RegistryMsg>(4096);
        let (ticker, _task) = Ticker::spawn(30.0, 64).expect("rate");
        let (m_tx, metrics) = channel::<MetricsEvent>(1 << 16);
        tokio::spawn(Registry::new(rx, reg.clone(), factory, ticker, m_tx, None, None, None).run());
        for id in ROOMS {
            let (reply, created) = oneshot::channel();
            let config = RoomConfig {
                id,
                tick_hz: 30.0,
                keepalive_hz: KEEPALIVE_HZ,
                metrics_cadence_hz: 30.0,
                ..Default::default()
            };
            reg.send(RegistryMsg::CreateRoom { config, reply })
                .await
                .expect("registry");
            created.await.expect("reply").expect("created");
        }
        Self {
            reg,
            metrics,
            shards,
            steps: HashMap::new(),
            pairs: Vec::new(),
            next_conn: 1,
            tick: 0,
            stats: (Stats::default(), Stats::default()),
        }
    }

    /// Apply one tick's operations to both rooms, then step.
    pub(super) async fn play(&mut self, ops: &[Op]) {
        for op in ops {
            match op {
                Op::Join(identity) => self.join(identity).await,
                Op::Leave(i) => {
                    let (a, b) = self.pairs.remove(*i);
                    for conn in [a.conn, b.conn] {
                        self.reg
                            .send(RegistryMsg::DespawnPlayer { conn })
                            .await
                            .expect("registry");
                    }
                }
                Op::Move(i, x, y) => {
                    let (a, b) = &self.pairs[*i];
                    a.send(MOVE, to(*x, *y));
                    b.send(MOVE, to(*x, *y));
                }
                Op::Switch(i, team) => {
                    let (a, b) = &self.pairs[*i];
                    a.send(SWITCH, vec![*team].into());
                    b.send(SWITCH, vec![*team].into());
                }
            }
        }
        self.step().await;
    }

    async fn join(&mut self, identity: &str) {
        let a = self.spawn(ROOMS[0], identity).await;
        let b = self.spawn(ROOMS[1], identity).await;
        self.pairs.push((a, b));
    }

    async fn spawn<const RUN: bool>(&mut self, room: RoomId, identity: &str) -> Side<RUN> {
        let conn = ConnectionId(self.next_conn);
        self.next_conn += 1;
        let (out_tx, out) = channel::<FrameBatch>(1024);
        let (reply, joined) = oneshot::channel();
        self.reg
            .send(RegistryMsg::SpawnPlayer {
                conn,
                room,
                out: out_tx,
                identity: identity.to_string(),
                reply,
            })
            .await
            .expect("registry");
        Side {
            conn,
            out,
            joined: Some(joined),
            actions: None,
            view: ClientView::new(Dec::new(super::CELL)),
            frames: Vec::new(),
        }
    }

    /// One global tick: wait until every room actor / shard of both
    /// rooms finished it, let every client apply what it received, and
    /// compare the pairs.
    async fn step(&mut self) {
        let target = self.tick + 1;
        let want = 2 * self.shards as usize;
        while self.steps.len() < want || self.steps.values().any(|&s| s < target) {
            let event = tokio::time::timeout(WAIT, self.metrics.recv())
                .await
                .expect("a sample in time")
                .expect("metrics open");
            if let MetricsEvent::Room(s) = event {
                let key = if s.room.0 >= 1 << 16 {
                    (s.room.0 >> 16, s.room.0 & 0xffff)
                } else {
                    (s.room.0, 0)
                };
                let at = self.steps.entry(key).or_default();
                *at = (*at).max(s.steps);
            }
        }
        self.tick = target;
        for (a, b) in &mut self.pairs {
            settle(a).await;
            settle(b).await;
            a.drain(&mut self.stats.0);
            b.drain(&mut self.stats.1);
            same(self.tick, (&a.view, &a.frames), (&b.view, &b.frames));
        }
    }

    /// Every client's counters (the run side), summed.
    pub(super) fn counters(&self) -> Counters {
        let mut sum = Counters::default();
        for (_, b) in &self.pairs {
            add(&mut sum, b.view.counters());
        }
        sum
    }

    /// The live pairs.
    pub(super) fn players(&self) -> usize {
        self.pairs.len()
    }
}

/// Take the join's reply once it arrived (a join lands on the room's
/// next tick boundary — by the barrier after it).
async fn settle<const RUN: bool>(side: &mut Side<RUN>) {
    if let Some(joined) = side.joined.take() {
        let (_, actions) = tokio::time::timeout(WAIT, joined)
            .await
            .expect("joined in time")
            .expect("reply")
            .expect("accepted");
        side.actions = Some(actions);
    }
}
