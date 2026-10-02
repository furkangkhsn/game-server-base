//! Test harness: the war room as it runs in the server — a live
//! `gsb-core` registry (whose team hub relays the shards' exports) over
//! FOUR real shard actors, a global ticker on a paused clock, and
//! clients that apply what their out channel actually receives under the
//! kit's client rules. Public API only — the game's, the kit's and the
//! core's (the kit's own rig for these actors is crate-internal; this is
//! its equivalent from the outside).
//!
//! The step barrier is the metrics channel: every shard emits one sample
//! per step after its broadcast phase. On the paused clock the ticker
//! fires only when every task is idle — so once all four samples of a
//! tick are in, every frame of that tick sits in the clients' channels
//! and every export of that tick has been relayed.
//!
//! **Walking runs on the paused clock too.** The core's ticker stamps
//! each tick on the runtime clock (`tokio::time::Instant`), and the
//! shard's `dt` is the gap between two stamps: under tokio's paused
//! clock that is the tick period, so a runner covers its distance as on
//! the real clock — in milliseconds of real time. (Before BACKLOG F10
//! the stamps were wall-clock, `dt` froze, and the walking scenario ran
//! on the real clock.)

#![allow(dead_code)] // each test binary uses its own subset

mod client;

pub use client::Client;

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::channel::{FrameBatch, Inbox, Mailbox, channel};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::{MetricsEvent, RoomSample};
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory, Seat};
use gsb_core::room::RoomConfig;
use gsb_core::shard::ShardLogic;
use gsb_core::ticker::Ticker;
use gsb_demo_war::codec::WarWire;
use gsb_demo_war::combat::Hit;
use gsb_demo_war::world::{SHARDS, home_shard};
use gsb_demo_war::{Pos3, Realm, WarMig, war_shard};
use gsb_kit::sharded::TeamMig;
use gsb_kit::team::Team;
use tokio::sync::oneshot;

pub const ROOM: RoomId = RoomId(1);
const WAIT: Duration = Duration::from_secs(30);

type Logic = dyn ShardLogic<World, GroupKey = Team, State = TeamMig<WarMig>, Strip = WarWire>;

/// The room's factory: the four war shards over `realm`, every one
/// publishing its hits on `feed`, and the router the server module uses
/// (the shard owning the identity's placement).
fn factory(realm: Realm, feed: Mailbox<Hit>) -> RoomFactory<World, Team, TeamMig<WarMig>, WarWire> {
    Arc::new(move |_id, _cfg| {
        let shards = (0..SHARDS)
            .map(|i| {
                let mut shard = war_shard(i, &realm);
                shard.game_mut().set_combat_feed(feed.clone());
                (World::new(), Box::new(shard) as Box<Logic>)
            })
            .collect();
        let realm = realm.clone();
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |_conn, identity: &str| {
                home_shard(&realm.placement(identity).at)
            }),
        }
    })
}

/// The war room over a live registry.
pub struct War {
    reg: Mailbox<RegistryMsg>,
    metrics: Inbox<MetricsEvent>,
    hits: Inbox<Hit>,
    /// Steps each shard has reported.
    steps: [u64; SHARDS],
    /// Each shard's latest metrics sample.
    pub samples: [Option<RoomSample>; SHARDS],
    pub clients: Vec<Client>,
    /// The global tick of the latest barrier.
    pub tick: u64,
}

impl War {
    /// A live registry with the war room created over `realm`, a
    /// keep-alive full every 30 ticks.
    pub async fn new(realm: &Realm) -> Self {
        let (reg, rx) = channel::<RegistryMsg>(4096);
        let (ticker, _task) = Ticker::spawn(30.0, 64).expect("rate");
        let (m_tx, metrics) = channel::<MetricsEvent>(1 << 16);
        let (feed, hits) = channel::<Hit>(4096);
        tokio::spawn(
            Registry::new(
                rx,
                reg.clone(),
                factory(realm.clone(), feed),
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
            keepalive_hz: 1.0,
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
            hits,
            steps: [0; SHARDS],
            samples: [None; SHARDS],
            clients: Vec::new(),
            tick: 0,
        }
    }

    /// Log in on `conn` as `identity` (its placement decides the home
    /// shard); returns the client's index. The join settles within the
    /// tick it takes.
    pub async fn join(&mut self, conn: u64, identity: &str) -> usize {
        let (out_tx, out) = channel::<FrameBatch>(1024);
        let (reply, joined) = oneshot::channel();
        self.reg
            .send(RegistryMsg::SpawnPlayer {
                conn: ConnectionId(conn),
                room: ROOM,
                out: out_tx,
                identity: identity.to_string(),
                reply,
                claims: None,
            })
            .await
            .expect("registry");
        let Seat {
            entity: wire,
            actions,
            ..
        } = tokio::time::timeout(WAIT, joined)
            .await
            .expect("joined in time")
            .expect("reply")
            .expect("accepted");
        self.clients
            .push(Client::new(ConnectionId(conn), wire, out, actions));
        self.step().await;
        self.clients.len() - 1
    }

    /// Client `i` leaves (the registry's voluntary leave path; takes
    /// effect within a tick or two).
    pub async fn leave(&mut self, i: usize) {
        let conn = self.clients[i].conn;
        self.reg
            .send(RegistryMsg::DespawnPlayer { conn })
            .await
            .expect("registry");
    }

    /// One global tick: wait until every shard finished it, then let
    /// every client apply what it received.
    pub async fn step(&mut self) {
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
        }
    }

    fn sample(&mut self, event: MetricsEvent) {
        if let MetricsEvent::Room(s) = event {
            let i = (s.room.0 & 0xffff) as usize;
            self.steps[i] = self.steps[i].max(s.steps);
            self.samples[i] = Some(s);
        }
    }

    /// [`Self::step`] `n` times.
    pub async fn steps(&mut self, n: u32) {
        for _ in 0..n {
            self.step().await;
        }
    }

    /// Step until `done` holds (at most `limit` ticks); `false` if it
    /// never did.
    pub async fn until(&mut self, limit: u32, done: impl Fn(&War) -> bool) -> bool {
        for _ in 0..limit {
            if done(self) {
                return true;
            }
            self.step().await;
        }
        done(self)
    }

    /// Members (sessions) each shard held at its latest sample.
    pub fn members(&self) -> [u32; SHARDS] {
        self.samples.map(|s| s.map_or(0, |s| s.members))
    }

    /// The hits applied since the last call, in feed order.
    pub fn hits(&mut self) -> Vec<Hit> {
        let mut out = Vec::new();
        while let Ok(h) = self.hits.try_recv() {
            out.push(h);
        }
        out
    }

    /// Client `i`'s own wire id.
    pub fn wire(&self, i: usize) -> u64 {
        self.clients[i].wire
    }
}

/// A realm with saved characters `(identity, faction, x, z)`.
pub fn realm(saved: &[(&str, u8, f32, f32)]) -> Realm {
    saved.iter().fold(Realm::empty(), |r, &(id, f, x, z)| {
        r.with_login(id, Team(f), Pos3::ground(x, z))
    })
}
