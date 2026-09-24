//! Test harness: the MMO room as FOUR real `gsb-core` shard actors (the
//! registry's wiring: each shard's neighbour slots hold the neighbours'
//! mailboxes, the rest a dummy), one manually fed global ticker, and
//! clients that decode what their out channel actually receives under
//! the kit's client rules (full / delta / cell exits / one-shot private
//! full). Public API only — the MMO's, the kit's and the core's.
//!
//! The step barrier is the metrics channel: every shard emits one sample
//! per step (`metrics_cadence_hz == tick_hz`) AFTER its broadcast phase,
//! so once all four samples of a tick are in, every frame of that tick
//! sits in the clients' channels.

#![allow(dead_code)] // each test binary uses its own subset

mod client;

pub use client::Client;

use std::time::{Duration, Instant};

use bevy_ecs::prelude::World;
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::metrics::{MetricsEvent, RoomSample};
use gsb_core::room::{Action, ExpireTo, RoomConfig};
use gsb_core::shard::{ShardActor, ShardLogic, ShardMsg};
use gsb_core::ticker::TickInfo;
use gsb_demo_mmo::codec::MmoWire;
use gsb_demo_mmo::world::{SHARDS, home_shard};
use gsb_demo_mmo::{MmoMig, MmoShard, Pos3, Realm, mmo_shard};
use gsb_kit::sharded::KitMig;
use tokio::sync::{broadcast, mpsc, oneshot};

type Msg = ShardMsg<KitMig<MmoMig>, MmoWire>;

const WAIT: Duration = Duration::from_secs(5);
pub const TICK_HZ: f64 = 30.0;

/// The MMO room: four shard actors over one ticker.
pub struct Mmo {
    tick_tx: broadcast::Sender<TickInfo>,
    shards: Vec<Mailbox<Msg>>,
    metrics: Vec<mpsc::Receiver<MetricsEvent>>,
    /// Each shard's latest metrics sample (members, detached, expiries).
    pub samples: Vec<Option<RoomSample>>,
    logins: std::collections::HashMap<ConnectionId, Pos3>,
    t0: Instant,
    /// The last global tick fed.
    pub tick: u64,
}

impl Mmo {
    /// The room over `realm`, with the MMO's own disconnect grace and a
    /// 2 Hz keep-alive full (every 15 ticks).
    pub fn new(realm: &Realm) -> Self {
        Self::with(realm, gsb_demo_mmo::LOGOUT_GRACE, 2.0)
    }

    /// The room over `realm` with disconnect `grace` (ending the MMO's
    /// way: the slot is released) and `keepalive_hz`.
    pub fn with(realm: &Realm, grace: Duration, keepalive_hz: f64) -> Self {
        Self::build(realm, keepalive_hz, |s| s.with_disconnect_grace(grace))
    }

    /// The room over `realm` with a disconnect hold of `grace` that ends
    /// toward `to`, and `keepalive_hz`.
    pub fn with_policy(realm: &Realm, grace: Duration, to: ExpireTo, keepalive_hz: f64) -> Self {
        Self::build(realm, keepalive_hz, |s| {
            s.with_disconnect_policy(Some(grace), to)
        })
    }

    fn build(realm: &Realm, keepalive_hz: f64, policy: impl Fn(MmoShard) -> MmoShard) -> Self {
        let config = RoomConfig {
            id: RoomId(7),
            tick_hz: TICK_HZ,
            keepalive_hz,
            metrics_cadence_hz: TICK_HZ, // one sample per step: the barrier
            ..Default::default()
        };
        let (tick_tx, _) = broadcast::channel(64);
        let (dummy, _dummy_rx) = channel::<Msg>(1);
        let mut txs = Vec::new();
        let mut rxs = Vec::new();
        for _ in 0..SHARDS {
            let (tx, rx) = channel::<Msg>(config.control_capacity);
            txs.push(tx);
            rxs.push(rx);
        }
        let mut metrics = Vec::new();
        for (i, rx) in rxs.into_iter().enumerate() {
            let logic = policy(mmo_shard(i, realm));
            let links = (0..SHARDS)
                .map(|j| {
                    let near = logic.neighbors().contains(&j);
                    if near { txs[j].clone() } else { dummy.clone() }
                })
                .collect();
            let (m_tx, m_rx) = mpsc::channel(256);
            metrics.push(m_rx);
            let actor = ShardActor::new(
                config.clone(),
                i,
                World::new(),
                Box::new(logic),
                tick_tx.subscribe(),
                rx,
                links,
                1, // shard rate == global rate
                m_tx,
                None,
            );
            tokio::spawn(actor.run());
        }
        Self {
            tick_tx,
            shards: txs,
            metrics,
            samples: vec![None; SHARDS],
            logins: realm.logins.clone(),
            t0: Instant::now(),
            tick: 0,
        }
    }

    /// Feed one tick, wait until every shard finished it, then let every
    /// client decode what it received.
    pub async fn step(&mut self, clients: &mut [Client]) {
        self.tick += 1;
        let at = self.t0 + Duration::from_secs_f64(self.tick as f64 / TICK_HZ);
        self.tick_tx
            .send(TickInfo {
                tick: self.tick,
                at,
            })
            .expect("shards subscribed");
        for (i, rx) in self.metrics.iter_mut().enumerate() {
            match tokio::time::timeout(WAIT, rx.recv()).await {
                Ok(Some(MetricsEvent::Room(s))) => self.samples[i] = Some(s),
                other => panic!("shard {i} missed the step barrier: {other:?}"),
            }
        }
        for c in clients.iter_mut() {
            c.drain();
        }
    }

    /// [`Self::step`] `n` times.
    pub async fn steps(&mut self, clients: &mut [Client], n: u32) {
        for _ in 0..n {
            self.step(clients).await;
        }
    }

    /// Members (sessions) each shard holds right now.
    pub fn members(&self) -> Vec<u32> {
        self.samples
            .iter()
            .map(|s| s.as_ref().map_or(0, |s| s.members))
            .collect()
    }

    /// A shard's latest sample.
    pub fn sample(&self, shard: usize) -> &RoomSample {
        self.samples[shard].as_ref().expect("a stepped shard")
    }

    /// Log in on `conn` (its saved character decides the home shard, as
    /// the server's join router would), under resume key `identity`.
    /// Joins take effect on the next tick; `others` keep decoding.
    pub async fn join(&mut self, conn: u64, identity: &str, others: &mut [Client]) -> Client {
        let conn = ConnectionId(conn);
        let shard = self.logins.get(&conn).map_or(0, home_shard);
        let (out, rx) = mpsc::channel::<FrameBatch>(256);
        let (reply, reply_rx) = oneshot::channel();
        self.shards[shard]
            .send(ShardMsg::Join {
                conn,
                epoch: 1,
                identity: identity.to_string(),
                out,
                reply,
            })
            .await
            .expect("shard alive");
        self.step(others).await;
        let (id, actions) = tokio::time::timeout(WAIT, reply_rx)
            .await
            .expect("join reply timed out")
            .expect("reply dropped")
            .expect("join accepted");
        Client::new(conn, id, rx, actions)
    }

    /// The transport of `client` died (the registry broadcasts the
    /// detach to every shard; the owner runs the park policy).
    pub async fn detach(&mut self, client: &Client, identity: &str, others: &mut [Client]) {
        for s in &self.shards {
            s.send(ShardMsg::Detach {
                conn: client.conn,
                entity: client.id,
                identity: identity.to_string(),
            })
            .await
            .expect("shard alive");
        }
        self.step(others).await;
    }

    /// A new session on `conn` resumes `identity` (broadcast to every
    /// shard, as the registry does). `Some(client)` when a shard's park
    /// ledger held it; `None` when none did.
    pub async fn resume(
        &mut self,
        conn: u64,
        epoch: u64,
        identity: &str,
        others: &mut [Client],
    ) -> Option<(usize, Client)> {
        let conn = ConnectionId(conn);
        let mut pending = Vec::new();
        for s in &self.shards {
            let (out, rx) = mpsc::channel::<FrameBatch>(256);
            let (reply, reply_rx) = oneshot::channel();
            s.send(ShardMsg::Resume {
                conn,
                epoch,
                identity: identity.to_string(),
                out,
                reply,
            })
            .await
            .expect("shard alive");
            pending.push((rx, reply_rx));
        }
        self.step(others).await;
        let mut won = None;
        for (i, (rx, reply_rx)) in pending.into_iter().enumerate() {
            let answer: Result<Option<(EntityId, Mailbox<Action>)>, _> =
                tokio::time::timeout(WAIT, reply_rx)
                    .await
                    .expect("resume reply timed out")
                    .expect("reply dropped");
            if let Ok(Some((id, actions))) = answer {
                assert!(won.is_none(), "two shards accepted one resume");
                won = Some((i, Client::new(conn, id, rx, actions)));
            }
        }
        won
    }
}
