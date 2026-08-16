//! Regression tests for the broadcast phase.
//!
//! 1. A connection that joins a room where others are already present must
//!    receive the **entire world** (full snapshot) on the next broadcast —
//!    not only what changes afterwards. (Previously `last_sent` was global
//!    per room, so a late joiner saw nothing until each entity moved.)
//! 2. A *stale* leave (its connection re-joined before the leave was
//!    processed) must not despawn the new entity.
//!
//! Ticks are driven manually over the global broadcast channel with exact
//! synthetic timestamps, so every step's `dt` is exactly one period
//! (deterministic, independent of wall-clock speed).

use std::time::{Duration, Instant};

use bevy_ecs::prelude::World;
use bytes::Bytes;
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::room::{Action, RoomActor, RoomConfig, RoomControl};
use gsb_core::ticker::TickInfo;
use gsb_protocol::FrameBody;
use tokio::sync::{broadcast, mpsc, oneshot};

use gsb_game::op;
use prost::Message;

const WAIT: Duration = Duration::from_secs(5);
/// Nominal period for synthetic timestamps (the room default is 30 Hz).
const PERIOD_SECS: f64 = 1.0 / 30.0;

async fn reply<T>(rx: oneshot::Receiver<T>) -> T {
    tokio::time::timeout(WAIT, rx)
        .await
        .expect("timed out waiting for reply")
        .expect("reply dropped")
}

async fn next_batch(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<FrameBody> {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("timed out waiting for a batch")
        .expect("out channel closed")
}

fn states(batch: &[FrameBody]) -> Vec<gsb_game::game::EntityState> {
    batch
        .iter()
        .filter(|f| f.op == op::ENTITY_STATE)
        .map(|f| {
            gsb_game::game::EntityState::decode(f.payload.as_ref())
                .expect("bad ENTITY_STATE payload")
        })
        .collect()
}

/// A room actor driven by a manually fed global ticker: each tick carries
/// `at = t0 + n * period`, so the room's dt is exactly one period.
struct TestRoom {
    tick_tx: broadcast::Sender<TickInfo>,
    control: Mailbox<RoomControl>,
    handle: tokio::task::JoinHandle<()>,
    t0: Instant,
    next_tick: u64,
}

impl TestRoom {
    fn new() -> Self {
        let config = RoomConfig {
            id: RoomId(1),
            ..Default::default()
        };
        let (tick_tx, tick_rx) = broadcast::channel(64);
        let (control, control_rx) = channel(config.control_capacity);
        let actor = RoomActor::new(
            config,
            World::new(),
            Box::new(gsb_game::room::DemoRoom::new()),
            tick_rx,
            control_rx,
            1, // room rate == global rate in these tests
        );
        Self {
            tick_tx,
            control,
            handle: tokio::spawn(actor.run()),
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

    /// Buffer the join, then feed one tick: the room processes it (control
    /// is handled on the step tick) and the reply is ready afterwards.
    async fn join(
        &mut self,
        conn: ConnectionId,
    ) -> (EntityId, mpsc::Receiver<FrameBatch>, Mailbox<Action>) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) = oneshot::channel::<(EntityId, Mailbox<Action>)>();
        self.control
            .send(RoomControl::Join {
                conn,
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control channel alive");
        self.tick();
        let (entity, actions) = reply(reply_rx).await;
        (entity, out_rx, actions)
    }

    async fn shutdown(mut self) {
        self.control
            .send(RoomControl::Shutdown)
            .await
            .expect("control channel alive");
        self.tick();
        tokio::time::timeout(WAIT, self.handle)
            .await
            .expect("room did not shut down")
            .expect("room task panicked");
    }
}

#[tokio::test]
async fn late_joiner_receives_full_world_snapshot() {
    let mut room = TestRoom::new();

    // A joins and then stays perfectly still.
    let c_a = ConnectionId(1);
    let (a_entity, mut a_rx, a_actions) = room.join(c_a).await;
    let batch = next_batch(&mut a_rx).await;
    assert!(
        states(&batch).iter().any(|s| s.entity == a_entity),
        "A must see its own entity"
    );

    // B joins late, while A is still.
    let c_b = ConnectionId(2);
    let (b_entity, mut b_rx, _b_actions) = room.join(c_b).await;
    assert_ne!(a_entity, b_entity);

    // The join tick's broadcast: B must see both A (existing, still) and itself.
    let batch = next_batch(&mut b_rx).await;
    let seen: Vec<u64> = states(&batch).iter().map(|s| s.entity).collect();
    assert!(
        seen.contains(&a_entity),
        "late joiner did not see the existing still entity: {seen:?}"
    );
    assert!(
        seen.contains(&b_entity),
        "late joiner must see its own entity"
    );

    // A moves (over its per-connection action channel); B must observe the
    // version bump.
    let move_to = gsb_game::game::MoveTo { x: 10, y: 10 };
    a_actions
        .send(Action {
            conn: c_a,
            op: op::MOVE_TO,
            payload: Bytes::from(move_to.encode_to_vec()),
        })
        .await
        .expect("action channel alive");
    room.tick();
    let batch = next_batch(&mut b_rx).await;
    let a_state = states(&batch)
        .into_iter()
        .find(|s| s.entity == a_entity)
        .expect("B must receive A's updated state");
    assert!(
        a_state.version > 0,
        "moved entity must carry a bumped version"
    );

    room.shutdown().await;
}

#[tokio::test]
async fn stale_leave_cannot_kill_rejoined_entity() {
    let mut room = TestRoom::new();

    // A joins → entity E1.
    let c_a = ConnectionId(3);
    let (e1, mut a_rx, _a_actions) = room.join(c_a).await;
    let _ = next_batch(&mut a_rx).await;

    // A leaves, then re-joins: the join replaces the stale state and
    // creates E2 (both processed in the same tick's control phase, in order).
    room.control
        .send(RoomControl::Leave {
            conn: c_a,
            entity: e1,
        })
        .await
        .expect("control channel alive");
    // The rejoin gets a fresh out channel (the room's old one is dropped).
    let (e2, mut a_rx, a_actions) = room.join(c_a).await;
    assert_ne!(e1, e2, "rejoin must create a fresh entity");
    // Consume the rejoin tick's batch (E2 at v0, plus the E1 removed event).
    let _ = next_batch(&mut a_rx).await;

    // A *stale* leave for E1 arrives late: it must be ignored.
    room.control
        .send(RoomControl::Leave {
            conn: c_a,
            entity: e1,
        })
        .await
        .expect("control channel alive");

    // If E2 had been killed by the stale leave, A's MOVE_TO below would be
    // dropped (no live entity) and this batch would never arrive.
    let move_to = gsb_game::game::MoveTo { x: -20, y: 20 };
    a_actions
        .send(Action {
            conn: c_a,
            op: op::MOVE_TO,
            payload: Bytes::from(move_to.encode_to_vec()),
        })
        .await
        .expect("action channel alive");
    room.tick();
    let batch = next_batch(&mut a_rx).await;
    let state = states(&batch)
        .into_iter()
        .find(|s| s.entity == e2)
        .expect("rejoined entity E2 must survive the stale leave");
    assert!(state.version > 0);

    room.shutdown().await;
}
