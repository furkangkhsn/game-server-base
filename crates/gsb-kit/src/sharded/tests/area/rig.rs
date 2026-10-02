//! The actor rig of the seam queries: a live registry over four real
//! shard actors running the plain [`ShardedRoom`] on a 2×2 grid with the
//! 8-neighbourhood (the corner regions lend to each other, so an entity
//! handed between two shards is seen by a third from both), a global
//! ticker on a paused clock, and the [`Sweep`] game, whose systems ask
//! the seam for a disc around the map's centre every tick and report
//! the answer. The step barrier is the metrics channel (one sample per
//! shard and step, after its broadcast phase).

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::channel::{FrameBatch, Inbox, Mailbox, channel};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory, Seat};
use gsb_core::room::{Action, RoomConfig};
use gsb_core::shard::ShardLogic;
use gsb_core::ticker::Ticker;
use tokio::sync::oneshot;

use super::sweep::{MOVE, Sample, Sweep, parse};
use crate::sharded::{KitMig, ShardedRoom};
use crate::space::{GridPartition2, Partition};
use crate::testing::{FixMig, Position, WirePos};

/// The map spans `[-HALF, HALF]²`: shard 0 is x < 0, y < 0; 1 is x ≥ 0,
/// y < 0; 2 is x < 0, y ≥ 0; 3 is x ≥ 0, y ≥ 0. Border margin 25.
const HALF: f32 = 100.0;
const SHARDS: usize = 4;
const ROOM: RoomId = RoomId(1);
const WAIT: Duration = Duration::from_secs(30);

type Factory = RoomFactory<World, (), KitMig<FixMig>, WirePos>;

fn factory(feed: Mailbox<Sample>) -> Factory {
    Arc::new(move |_id, _cfg| {
        let partition = || GridPartition2::<Position>::new(SHARDS, HALF).with_diagonals();
        let shards = (0..SHARDS)
            .map(|index| {
                let game = Sweep::new(index, feed.clone());
                let room = ShardedRoom::with_game(game, partition(), index);
                let logic: Box<
                    dyn ShardLogic<World, GroupKey = (), State = KitMig<FixMig>, Strip = WirePos>,
                > = Box::new(room);
                (World::new(), logic)
            })
            .collect();
        let grid = partition();
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |_conn, identity: &str| {
                Partition::<WirePos>::region_of(&grid, &parse(identity))
            }),
        }
    })
}

/// The room, its players and every probe answer so far.
pub(super) struct Rig {
    reg: Mailbox<RegistryMsg>,
    metrics: Inbox<MetricsEvent>,
    feed: Inbox<Sample>,
    steps: Vec<u64>,
    players: Vec<(ConnectionId, Mailbox<Action>, Inbox<FrameBatch>)>,
    pub(super) samples: Vec<Sample>,
    /// The global tick of the latest barrier.
    pub(super) tick: u64,
}

impl Rig {
    pub(super) async fn new() -> Self {
        let (feed_tx, feed) = channel::<Sample>(1 << 12);
        let (reg, rx) = channel::<RegistryMsg>(4096);
        let (ticker, _task) = Ticker::spawn(30.0, 64).expect("rate");
        let (m_tx, metrics) = channel::<MetricsEvent>(1 << 16);
        let registry = Registry::new(
            rx,
            reg.clone(),
            factory(feed_tx),
            ticker,
            m_tx,
            None,
            None,
            None,
        );
        tokio::spawn(registry.run());
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
            feed,
            steps: vec![0; SHARDS],
            players: Vec::new(),
            samples: Vec::new(),
            tick: 0,
        }
    }

    /// Log in at `x:y` on `conn`; the player's wire id.
    pub(super) async fn join(&mut self, conn: u64, identity: &str) -> u64 {
        let (out_tx, out) = channel::<FrameBatch>(1024);
        let (reply, joined) = oneshot::channel();
        let identity = identity.to_string();
        let conn = ConnectionId(conn);
        let msg = RegistryMsg::SpawnPlayer {
            conn,
            room: ROOM,
            out: out_tx,
            identity,
            reply,
            claims: None,
        };
        self.reg.send(msg).await.expect("registry");
        let Seat {
            entity, actions, ..
        } = tokio::time::timeout(WAIT, joined)
            .await
            .expect("joined in time")
            .expect("reply")
            .expect("accepted");
        self.players.push((conn, actions, out));
        self.step().await;
        entity
    }

    /// Player `i` (join order) teleports to `(x, y)`.
    pub(super) fn teleport(&self, i: usize, x: f32, y: f32) {
        let mut payload = x.to_le_bytes().to_vec();
        payload.extend_from_slice(&y.to_le_bytes());
        let (conn, actions, _) = &self.players[i];
        let action = Action {
            conn: *conn,
            player: PlayerId(0),
            op: MOVE,
            payload: payload.into(),
        };
        actions.try_send(action).expect("room");
    }

    /// One global tick: every shard finished it (and reported its probe).
    pub(super) async fn step(&mut self) {
        let target = self.steps.iter().copied().max().unwrap_or(0) + 1;
        loop {
            while let Ok(event) = self.metrics.try_recv() {
                self.count(event);
            }
            if self.steps.iter().all(|&s| s >= target) {
                break;
            }
            let event = tokio::time::timeout(WAIT, self.metrics.recv())
                .await
                .expect("a sample in time")
                .expect("metrics open");
            self.count(event);
        }
        self.tick = target;
        while let Ok(sample) = self.feed.try_recv() {
            self.samples.push(sample);
        }
        for (_, _, out) in &mut self.players {
            while out.try_recv().is_ok() {}
        }
    }

    fn count(&mut self, event: MetricsEvent) {
        if let MetricsEvent::Room(s) = event {
            let i = (s.room.0 & 0xffff) as usize;
            self.steps[i] = self.steps[i].max(s.steps);
        }
    }

    pub(super) async fn steps(&mut self, n: u32) {
        for _ in 0..n {
            self.step().await;
        }
    }
}
