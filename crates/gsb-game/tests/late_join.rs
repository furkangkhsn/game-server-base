//! Regression tests for the broadcast phase (per-group full snapshots).
//!
//! 1. A connection that joins a room where others are already present must
//!    receive the **entire world** (full snapshot) on the next broadcast —
//!    not only what changes afterwards. Membership is expressed by
//!    presence in the snapshot.
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
use gsb_game::game::WorldSnapshot;
use gsb_protocol::FrameBody;
use prost::Message;
use tokio::sync::{broadcast, mpsc, oneshot};

use gsb_game::op;

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

/// The world snapshot carried by a batch (each batch holds at most one —
/// one snapshot per group per tick, and the demo has a single group).
fn snapshot(batch: &[FrameBody]) -> WorldSnapshot {
    let frame = batch
        .iter()
        .find(|f| f.op == op::WORLD_SNAPSHOT)
        .expect("batch must carry a WORLD_SNAPSHOT frame");
    WorldSnapshot::decode(frame.payload.as_ref()).expect("bad WORLD_SNAPSHOT payload")
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
        let (metrics_tx, _metrics_rx) = mpsc::unbounded_channel::<gsb_core::metrics::MetricsEvent>();
        let actor = RoomActor::new(
            config,
            World::new(),
            Box::new(gsb_game::room::DemoRoom::new()),
            tick_rx,
            control_rx,
            1, // room rate == global rate in these tests
            metrics_tx,
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
    let snap = snapshot(&batch);
    assert!(
        snap.entities.iter().any(|e| e.entity == a_entity),
        "A must see its own entity"
    );

    // B joins late, while A is still.
    let c_b = ConnectionId(2);
    let (b_entity, mut b_rx, _b_actions) = room.join(c_b).await;
    assert_ne!(a_entity, b_entity);

    // The join tick's broadcast: B must see both A (existing, still) and
    // itself — the join is processed in the control phase, before this
    // tick's snapshot, so one full snapshot covers both.
    let batch = next_batch(&mut b_rx).await;
    let join_snap = snapshot(&batch);
    let seen: Vec<u64> = join_snap.entities.iter().map(|e| e.entity).collect();
    assert!(
        seen.contains(&a_entity),
        "late joiner did not see the existing still entity: {seen:?}"
    );
    assert!(
        seen.contains(&b_entity),
        "late joiner must see its own entity"
    );
    // Record A's position as B sees it; after the move below, B must
    // observe a different one.
    let a_start = join_snap
        .entities
        .iter()
        .find(|e| e.entity == a_entity)
        .expect("A's entity in the join-tick snapshot");

    // A moves (over its per-connection action channel); the world changes,
    // so the group snapshot is re-emitted — B must observe the new
    // position. But emission is gated on the *wire* content: at 10 units/s,
    // 30 Hz the integer position changes within a few ticks, yet the ticks
    // in between are legitimately silent (no wire change ⇒ no snapshot).
    // So feed ticks and take a batch *whenever one arrives* — not on every
    // tick — until B sees the movement.

    let move_to = gsb_game::game::MoveTo {
        x: a_start.x + 100,
        y: a_start.y + 100,
    };
    a_actions
        .send(Action {
            conn: c_a,
            op: op::MOVE_TO,
            payload: Bytes::from(move_to.encode_to_vec()),
        })
        .await
        .expect("action channel alive");

    let mut a_moved = None;
    for _ in 0..60 {
        room.tick();
        // Yield so the room (same runtime) finishes this step's fan-out.
        tokio::time::sleep(Duration::from_millis(10)).await;
        while let Ok(batch) = b_rx.try_recv() {
            let snap = snapshot(&batch);
            let rec = snap
                .entities
                .iter()
                .find(|e| e.entity == a_entity)
                .expect("A's entity must stay in the snapshot");
            if (rec.x, rec.y) != (a_start.x, a_start.y) {
                a_moved = Some((rec.x, rec.y));
            }
        }
        if a_moved.is_some() {
            break;
        }
    }
    assert!(
        a_moved.is_some(),
        "B must observe A's moved position in a snapshot (start {:?})",
        (a_start.x, a_start.y)
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
    // Consume the rejoin tick's batch: E2 is in the snapshot, E1 is not
    // (membership = presence in the snapshot).
    let batch = next_batch(&mut a_rx).await;
    let seen: Vec<u64> = snapshot(&batch)
        .entities
        .iter()
        .map(|e| e.entity)
        .collect();
    assert!(seen.contains(&e2), "rejoined entity E2 must be in the snapshot");
    assert!(!seen.contains(&e1), "left entity E1 must be out of the snapshot");

    // A *stale* leave for E1 arrives late: it must be ignored.
    room.control
        .send(RoomControl::Leave {
            conn: c_a,
            entity: e1,
        })
        .await
        .expect("control channel alive");

    // If E2 had been killed by the stale leave, A's MOVE_TO below would be
    // dropped (no live entity) and no snapshot carrying E2 would ever
    // arrive. The "no change" detector compares wire content, so the first
    // sub-integer movement ticks emit nothing — feed ticks until the next
    // snapshot arrives (the integer position changes within a few ticks at
    // 10 units/s, 30 Hz).
    let move_to = gsb_game::game::MoveTo { x: -20, y: 20 };
    a_actions
        .send(Action {
            conn: c_a,
            op: op::MOVE_TO,
            payload: Bytes::from(move_to.encode_to_vec()),
        })
        .await
        .expect("action channel alive");
    let mut batch = None;
    for _ in 0..60 {
        room.tick();
        // Yield so the room (same runtime) finishes this step's fan-out.
        tokio::time::sleep(Duration::from_millis(10)).await;
        if let Ok(b) = a_rx.try_recv() {
            batch = Some(b);
            break;
        }
    }
    let batch = batch.expect("E2 must produce a snapshot after MOVE_TO");
    let seen: Vec<u64> = snapshot(&batch)
        .entities
        .iter()
        .map(|e| e.entity)
        .collect();
    assert!(
        seen.contains(&e2),
        "rejoined entity E2 must survive the stale leave"
    );
    assert!(!seen.contains(&e1), "stale-leave victim E1 must not reappear");

    room.shutdown().await;
}
