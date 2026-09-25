//! Test harness: an arena room inside the REAL `gsb-core` room actor,
//! driven by a manually fed global ticker, with clients that decode what
//! their out channel actually receives (public API only — the arena's
//! and the kit's public surface, the core's actor).

#![allow(dead_code)] // each test binary uses its own subset

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bevy_ecs::prelude::World;
use bytes::Bytes;
use gsb_core::PlayerId;
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::room::{Action, RoomActor, RoomConfig, RoomControl};
use gsb_core::ticker::TickInfo;
use gsb_demo_arena::arena::{self, Private, WorldSnapshot, private};
use gsb_demo_arena::codec::{Cm3, to_cm};
use gsb_demo_arena::{ArenaGame, arena_room, op};
use prost::Message;
use tokio::sync::{broadcast, mpsc, oneshot};

const WAIT: Duration = Duration::from_secs(5);
const PERIOD_SECS: f64 = 1.0 / 30.0;

/// Ticks that settle any move inside the arena: the longest path (a
/// base to the far side and up, < 110 m) at 12 m/s is < 280 ticks.
pub const SETTLE: u32 = 300;

/// One connected player: its unit's wire id, and what its connection
/// has received so far.
pub struct Client {
    pub conn: ConnectionId,
    /// The unit's wire id (JOIN_ROOM_RESULT's value).
    pub id: u64,
    rx: mpsc::Receiver<FrameBatch>,
    actions: Mailbox<Action>,
    /// The latest team snapshot's content: wire id → record (a full
    /// snapshot replaces the view).
    pub view: BTreeMap<u64, Cm3>,
    /// Every input ack received, in order.
    pub acks: Vec<u64>,
    /// The raw payloads of the latest snapshot and private frames.
    pub last_snapshot: Option<Bytes>,
    pub last_private: Option<Bytes>,
    /// The raw payload of the first private frame.
    pub first_private: Option<Bytes>,
    /// Every session payload (`Private.game`) received, in order.
    pub welcomes: Vec<arena::Welcome>,
}

impl Client {
    /// The wire ids in the current view.
    pub fn sees(&self) -> Vec<u64> {
        self.view.keys().copied().collect()
    }

    /// Send a `MoveTo` (metres; `seq` 0 = unnumbered).
    pub async fn move_to(&self, x: f32, y: f32, z: f32, seq: u64) {
        let msg = arena::MoveTo {
            x: to_cm(x),
            y: to_cm(y),
            z: to_cm(z),
            seq,
        };
        self.actions
            .send(Action {
                // The core stamps the stable player id from its binding.
                player: PlayerId(self.conn.0),
                conn: self.conn,
                op: op::ARENA_MOVE_TO,
                payload: Bytes::from(msg.encode_to_vec()),
            })
            .await
            .expect("action channel alive");
    }

    /// Decode everything the connection has received since the last call.
    fn drain(&mut self) {
        while let Ok(batch) = self.rx.try_recv() {
            let mut privates = 0;
            for f in batch.iter() {
                match f.op {
                    op::ARENA_SNAPSHOT => {
                        let s = WorldSnapshot::decode(f.payload.as_ref()).expect("snapshot");
                        assert!(!s.delta, "the team-fog room sends full snapshots");
                        self.view = s
                            .entities
                            .iter()
                            .map(|r| {
                                (
                                    r.entity,
                                    Cm3 {
                                        x: r.x,
                                        y: r.y,
                                        z: r.z,
                                    },
                                )
                            })
                            .collect();
                        self.last_snapshot = Some(f.payload.clone());
                    }
                    op::ARENA_PRIVATE => {
                        privates += 1;
                        let p = Private::decode(f.payload.as_ref()).expect("private");
                        if let Some(private::Payload::Ack(a)) = p.payload {
                            self.acks.push(a.processed_up_to);
                        }
                        self.welcomes.extend(p.game);
                        self.first_private.get_or_insert_with(|| f.payload.clone());
                        self.last_private = Some(f.payload.clone());
                    }
                    other => panic!("unexpected frame op {other}"),
                }
            }
            assert!(privates <= 1, "at most one private frame per tick");
        }
    }
}

/// An arena room in a real room actor.
pub struct Arena {
    tick_tx: broadcast::Sender<TickInfo>,
    control: Mailbox<RoomControl>,
    _handle: tokio::task::JoinHandle<()>,
    t0: Instant,
    next_tick: u64,
}

impl Arena {
    /// A room running `game` (the arena's own room constructor).
    pub fn new(game: ArenaGame) -> Self {
        let config = RoomConfig {
            id: RoomId(1),
            ..Default::default()
        };
        let (tick_tx, tick_rx) = broadcast::channel(64);
        let (control, control_rx) = channel(config.control_capacity);
        let (metrics_tx, _metrics_rx) = mpsc::channel::<gsb_core::metrics::MetricsEvent>(1);
        let actor = RoomActor::new(
            config,
            World::new(),
            Box::new(arena_room(game)),
            tick_rx,
            control_rx,
            1, // room rate == global rate
            metrics_tx,
            None,
        );
        Self {
            tick_tx,
            control,
            _handle: tokio::spawn(actor.run()),
            t0: Instant::now(),
            next_tick: 0,
        }
    }

    fn tick(&mut self) {
        self.next_tick += 1;
        let at = self.t0 + Duration::from_secs_f64(self.next_tick as f64 * PERIOD_SECS);
        self.tick_tx
            .send(TickInfo {
                tick: self.next_tick,
                at,
            })
            .expect("room subscriber alive");
    }

    /// Join on `conn` (joins take effect on the next tick).
    pub async fn join(&mut self, conn: u64) -> Client {
        let conn = ConnectionId(conn);
        let (out_tx, rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.control
            .send(RoomControl::Join {
                conn,
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control channel alive");
        self.tick();
        let (id, actions) = tokio::time::timeout(WAIT, reply_rx)
            .await
            .expect("timed out waiting for the join reply")
            .expect("reply dropped")
            .expect("join accepted");
        Client {
            conn,
            id,
            rx,
            actions,
            view: BTreeMap::new(),
            acks: Vec::new(),
            last_snapshot: None,
            last_private: None,
            first_private: None,
            welcomes: Vec::new(),
        }
    }

    /// Advance `ticks` steps, letting the room finish each step's fan-out
    /// and every client decode its channel after each one.
    pub async fn advance(&mut self, clients: &mut [Client], ticks: u32) {
        for _ in 0..ticks {
            self.tick();
            tokio::time::sleep(Duration::from_millis(1)).await;
            for c in clients.iter_mut() {
                c.drain();
            }
        }
    }
}

/// `ids` as a sorted list (for comparing against [`Client::sees`]).
pub fn set(ids: &[u64]) -> Vec<u64> {
    let mut v = ids.to_vec();
    v.sort_unstable();
    v
}
