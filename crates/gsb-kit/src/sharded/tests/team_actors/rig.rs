//! The rig: a live registry (with its team hub) over four real shard
//! actors running [`ShardedTeamRoom`] on the [`Front`] game, a global
//! ticker on a paused clock, and clients that apply what their out
//! channel receives under the kit's client rules.
//!
//! The step barrier is the metrics channel: every shard emits one sample
//! per step after its broadcast phase. On the paused clock the ticker
//! fires only when every task is idle — so by the time all four samples
//! of a tick are in, every frame of that tick sits in the clients'
//! channels and every export of that tick has been relayed.

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::channel::{FrameBatch, Inbox, Mailbox, channel};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::RoomConfig;
use gsb_core::shard::{ShardLogic, TeamExport};
use gsb_core::ticker::Ticker;
use tokio::sync::oneshot;

use super::client::Client;
use super::front::{Front, parse};
use crate::client::ClientView;
use crate::sharded::{ShardedRoom, ShardedTeamRoom, TeamMig};
use crate::space::{GridPartition2, Partition, VisionGrid2};
use crate::team::Team;
use crate::testing::{FixMig, Position, WirePos, fix_lent_pos};

/// The map spans `[-HALF, HALF]²`: shard 0 is x < 0, y < 0; 1 is x ≥ 0,
/// y < 0; 2 is x < 0, y ≥ 0; 3 is x ≥ 0, y ≥ 0. Border margin 25.
pub(super) const HALF: f32 = 100.0;
pub(super) const SHARDS: usize = 4;
pub(super) const RADIUS: f32 = 25.0;
pub(super) const ROOM: RoomId = RoomId(1);
const WAIT: Duration = Duration::from_secs(30);

type Shard = ShardedTeamRoom<Front, GridPartition2<Position>, VisionGrid2<Position>>;
type Factory = RoomFactory<World, Team, TeamMig<FixMig>, WirePos>;

/// The room.
pub(super) struct Rig {
    reg: Mailbox<RegistryMsg>,
    metrics: Inbox<MetricsEvent>,
    /// Steps each shard has reported.
    steps: [u64; SHARDS],
    /// Members each shard held at its latest sample.
    pub(super) members: [u32; SHARDS],
    pub(super) clients: Vec<Client>,
    /// The global tick of the latest barrier.
    pub(super) tick: u64,
}

fn factory(delta: bool) -> Factory {
    Arc::new(move |_id, _cfg| {
        let shards = (0..SHARDS)
            .map(|i| {
                let inner =
                    ShardedRoom::with_game(Front::default(), GridPartition2::new(SHARDS, HALF), i);
                let room = Shard::with_shard(inner, VisionGrid2::new(RADIUS), fix_lent_pos);
                let room = if delta { room.with_delta() } else { room };
                (
                    World::new(),
                    Box::new(room)
                        as Box<
                            dyn ShardLogic<
                                    World,
                                    GroupKey = Team,
                                    State = TeamMig<FixMig>,
                                    Strip = WirePos,
                                >,
                        >,
                )
            })
            .collect();
        let partition = GridPartition2::<Position>::new(SHARDS, HALF);
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |_conn, identity: &str| {
                let (_, x, y) = parse(identity);
                Partition::<WirePos>::region_of(&partition, &Position { x, y })
            }),
        }
    })
}

impl Rig {
    /// A live registry with the room created; `delta` = the delta mode.
    pub(super) async fn new(delta: bool) -> Self {
        let (reg, rx) = channel::<RegistryMsg>(4096);
        let (ticker, _task) = Ticker::spawn(30.0, 64).expect("rate");
        let (m_tx, metrics) = channel::<MetricsEvent>(1 << 16);
        tokio::spawn(
            Registry::new(
                rx,
                reg.clone(),
                factory(delta),
                ticker,
                m_tx,
                None,
                None,
                None,
            )
            .run(),
        );
        let (reply, created) = oneshot::channel();
        let config = RoomConfig {
            id: ROOM,
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 30.0,
            ..Default::default()
        };
        reg.send(RegistryMsg::CreateRoom { config, reply })
            .await
            .expect("registry");
        created.await.expect("reply").expect("created");
        Self {
            reg,
            metrics,
            steps: [0; SHARDS],
            members: [0; SHARDS],
            clients: Vec::new(),
            tick: 0,
        }
    }

    /// Log in as `identity` (`team:x:y`) on `conn`; returns the client's
    /// index. The join settles within a few ticks.
    pub(super) async fn join(&mut self, conn: u64, identity: &str) -> usize {
        let (out_tx, out) = channel::<FrameBatch>(1024);
        let (reply, joined) = oneshot::channel();
        self.reg
            .send(RegistryMsg::SpawnPlayer {
                conn: ConnectionId(conn),
                room: ROOM,
                out: out_tx,
                identity: identity.to_string(),
                reply,
            })
            .await
            .expect("registry");
        let (wire, actions) = tokio::time::timeout(WAIT, joined)
            .await
            .expect("joined in time")
            .expect("reply")
            .expect("accepted");
        self.clients.push(Client {
            conn: ConnectionId(conn),
            wire,
            out,
            actions,
            view: ClientView::default(),
            history: Vec::new(),
            doubled: 0,
        });
        self.step().await;
        self.clients.len() - 1
    }

    /// A voluntary leave (the registry's leave path).
    pub(super) async fn leave(&mut self, client: usize) {
        let conn = self.clients[client].conn;
        self.reg
            .send(RegistryMsg::DespawnPlayer { conn })
            .await
            .expect("registry");
    }

    /// Hand the hub a forged export, as shard `from` of the current
    /// incarnation would at the latest tick.
    pub(super) async fn forge(&mut self, from: usize, export: TeamExport) {
        self.forge_as(0, from, export).await;
    }

    /// [`Self::forge`] stamped with the install `generation` (the room's
    /// first incarnation is 0).
    pub(super) async fn forge_as(&mut self, generation: u64, from: usize, export: TeamExport) {
        self.reg
            .send(RegistryMsg::TeamExport {
                room: ROOM,
                generation,
                from,
                tick: self.tick,
                export,
            })
            .await
            .expect("registry");
    }

    /// One global tick: wait until every shard finished it, then let
    /// every client apply what it received. (The shards subscribe at
    /// creation, before the paused clock fires its first tick, so a
    /// shard's step count IS the global tick.)
    pub(super) async fn step(&mut self) {
        // Whatever finished while the test was elsewhere (a join's round
        // trip) is already queued: count it first.
        while let Ok(event) = self.metrics.try_recv() {
            self.sample(event);
        }
        let target = self.steps.iter().copied().max().unwrap_or(0) + 1;
        while self.steps.iter().any(|&s| s < target) {
            let event = tokio::time::timeout(WAIT, self.metrics.recv())
                .await
                .expect("a sample in time")
                .expect("metrics open");
            self.sample(event);
        }
        self.tick = target;
        for c in &mut self.clients {
            c.drain(target);
            assert!(
                c.view.last_sequence().unwrap_or(0) <= target,
                "a frame from the future: the step count is not the tick"
            );
        }
    }

    fn sample(&mut self, event: MetricsEvent) {
        if let MetricsEvent::Room(s) = event {
            let i = (s.room.0 & 0xffff) as usize;
            self.steps[i] = self.steps[i].max(s.steps);
            self.members[i] = s.members;
        }
    }

    /// [`Self::step`] `n` times.
    pub(super) async fn steps(&mut self, n: u32) {
        for _ in 0..n {
            self.step().await;
        }
    }
}
