//! Frame-rate independence: the simulation must produce the same
//! real-time displacement regardless of the tick rate.
//!
//! Two rooms under one 60 Hz global clock: room A steps at 60 Hz
//! (`run_every = 1`), room B at 15 Hz (`run_every = 4`). Both run the real
//! [`MovementSystem`] with an identical scenario (spawn at the origin,
//! target far away, constant speed). Over a window of 5.0 s of *simulated*
//! time the displacements must match — 300 fine steps on one side, 75 coarse
//! steps on the other.
//!
//! Positions are observed through a test-local `RoomLogic` that reports the
//! entity's f32 position as f64 over the wire (the demo `WORLD_SNAPSHOT`
//! records truncate positions to i32, which would hide the comparison
//! below the quantization noise).

use std::time::{Duration, Instant};

use bevy_ecs::prelude::{Entity, World};
use bytes::Bytes;
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::room::{Action, GameLogic, RoomActor, RoomConfig, RoomControl, RoomLogic, TickCtx};
use gsb_core::ticker::TickInfo;
use gsb_ecs::{System, SystemCtx};
use gsb_protocol::FrameBody;
use tokio::sync::{broadcast, mpsc, oneshot};

use gsb_game::components::{MoveTarget, Position, Speed};
use gsb_game::op;
use gsb_game::systems::MovementSystem;
use prost::Message;

const WAIT: Duration = Duration::from_secs(10);
const OBS_OP: u16 = 0x7F00;
/// How many global ticks to feed (600 ticks at 60 Hz = 10 s of sim time).
const TOTAL_TICKS: u64 = 600;
/// The measurement window: ticks 300..=600 = 5.0 s of sim time.
const FROM_TICK: u64 = 300;
const SPEED: f32 = 10.0;

/// A manual 60 Hz global clock with exact synthetic timestamps.
struct GlobalClock {
    tx: broadcast::Sender<TickInfo>,
    t0: Instant,
    next: u64,
}

impl GlobalClock {
    fn new() -> Self {
        let (tx, _first) = broadcast::channel(64);
        Self {
            tx,
            t0: Instant::now(),
            next: 0,
        }
    }

    fn tick(&mut self) {
        self.next += 1;
        let at = self.t0 + Duration::from_secs_f64(self.next as f64 / 60.0);
        self.tx
            .send(TickInfo {
                tick: self.next,
                at,
            })
            .expect("subscribers alive");
    }
}

/// Test room logic: one entity at the origin with a fixed speed; the first
/// MOVE_TO sets a far-away target; every step reports the position (f64).
///
/// One snapshot group (`()`): the room ships the group's snapshot once per
/// tick; this logic reports "changed" on every step (the entity moves
/// every step once the target is set, and even before that the test wants
/// exactly one batch per tick).
struct FrameLogic {
    entity: Option<Entity>,
}

// Faz 1 trait split: shared hooks on the `GameLogic` supertrait; no
// room-exclusive hook used (empty `RoomLogic` impl at the bottom).
impl GameLogic<World> for FrameLogic {
    type GroupKey = ();

    fn snapshot_op(&self) -> u16 {
        OBS_OP
    }
    fn private_op(&self) -> u16 {
        OBS_OP
    }

    fn group_of(&self, _world: &World, _conn: ConnectionId) -> Self::GroupKey {
        Default::default()
    }

    fn on_join(&mut self, world: &mut World, _conn: ConnectionId) -> EntityId {
        let e = world
            .spawn((Position { x: 0.0, y: 0.0 }, Speed(SPEED)))
            .id();
        self.entity = Some(e);
        e.to_bits()
    }

    fn on_leave(&mut self, world: &mut World, _conn: ConnectionId) {
        if let Some(e) = self.entity.take() {
            world.despawn(e);
        }
    }

    fn ingest(&mut self, world: &mut World, _ctx: &TickCtx, actions: &mut Vec<Action>) {
        for a in actions.drain(..) {
            if a.op != op::MOVE_TO {
                continue;
            }
            let Ok(m) = gsb_game::game::MoveTo::decode(&a.payload[..]) else {
                continue;
            };
            if let Some(e) = self.entity {
                world.entity_mut(e).insert(MoveTarget {
                    x: m.x as f32,
                    y: m.y as f32,
                });
            }
        }
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        MovementSystem.run(
            world,
            &SystemCtx {
                tick: ctx.tick,
                dt: ctx.dt.as_secs_f32(),
            },
        );
    }

    fn snapshot(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        _group: &Self::GroupKey,
        _borrowed: &[gsb_core::shard::BorrowedRecord],
        out: &mut bytes::BytesMut,
    ) -> bool {
        let Some(e) = self.entity else {
            return false; // nothing to observe (no joiner yet)
        };
        let (x, y) = {
            let ent = world.entity(e);
            let p = ent.get::<Position>().expect("position present");
            (p.x as f64, p.y as f64)
        };
        out.extend_from_slice(&x.to_bits().to_le_bytes());
        out.extend_from_slice(&y.to_bits().to_le_bytes());
        true
    }
}

impl RoomLogic<World> for FrameLogic {}

async fn next_batch(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<FrameBody> {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("timed out waiting for a batch")
        .expect("out channel closed")
}

fn observe(batch: &[FrameBody]) -> (f64, f64) {
    let frame = batch
        .iter()
        .find(|f| f.op == OBS_OP)
        .expect("batch must carry an observation frame");
    let x = u64::from_le_bytes(frame.payload[..8].try_into().expect("8 bytes"));
    let y = u64::from_le_bytes(frame.payload[8..16].try_into().expect("8 bytes"));
    (f64::from_bits(x), f64::from_bits(y))
}

/// A room under test: joined (control buffered), join reply + action mailbox
/// The join reply channel (the room may now structurally refuse a join —
/// e.g. at capacity — so the reply carries the outcome).
type JoinReplyRx =
    tokio::sync::oneshot::Receiver<Result<(EntityId, Mailbox<Action>), gsb_core::error::CoreError>>;

/// pending until the room's first step tick.
struct SimRoom {
    /// Kept alive so the room's control channel stays open (never read).
    #[allow(dead_code)]
    control: Mailbox<RoomControl>,
    out_rx: mpsc::Receiver<FrameBatch>,
    join_reply: Option<JoinReplyRx>,
    handle: tokio::task::JoinHandle<()>,
}

impl SimRoom {
    /// Buffer the join immediately; the room processes it on its first step
    /// tick (the reply then becomes ready).
    fn new(
        hz: f64,
        run_every: u64,
        tick_rx: broadcast::Receiver<TickInfo>,
        conn: ConnectionId,
    ) -> Self {
        let config = RoomConfig {
            id: RoomId(1),
            tick_hz: hz,
            ..Default::default()
        };
        let (control, control_rx) = channel(config.control_capacity);
        let (metrics_tx, _metrics_rx) = mpsc::channel::<gsb_core::metrics::MetricsEvent>(1);
        let actor = RoomActor::new(
            config,
            World::new(),
            Box::new(FrameLogic { entity: None }),
            tick_rx,
            control_rx,
            run_every,
            metrics_tx,
            None,
        );
        let handle = tokio::spawn(actor.run());
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), gsb_core::error::CoreError>>();
        // Bounded empty channel: the send cannot block; the room has not
        // stepped yet, so the join stays queued.
        control
            .try_send(RoomControl::Join {
                conn,
                out: out_tx,
                reply: reply_tx,
            })
            .expect("control channel accepts the join");
        SimRoom {
            control,
            out_rx,
            join_reply: Some(reply_rx),
            handle,
        }
    }

    /// Take the join reply if the room has processed it, then order the
    /// far-away move.
    fn complete_join(&mut self, conn: ConnectionId) -> bool {
        let Some(mut rx) = self.join_reply.take() else {
            return false;
        };
        let Ok(Ok((_entity, actions))) = rx.try_recv() else {
            // Not ready yet: the room has not stepped since the join was
            // buffered. Keep the receiver for the next tick.
            self.join_reply = Some(rx);
            return false;
        };
        let move_to = gsb_game::game::MoveTo { x: 1000, y: 1000, seq: 0 };
        // The join reply became ready *before* this tick's batch was flushed
        // (control phase precedes the broadcast phase), so the action
        // channel is open and empty: try_send cannot fail.
        actions
            .try_send(Action {
                conn,
                op: op::MOVE_TO,
                payload: Bytes::from(move_to.encode_to_vec()),
            })
            .expect("action channel accepts the move");
        // The action mailbox is dropped here on purpose: the test drives no
        // more input for this connection.
        true
    }
}

#[tokio::test]
async fn same_real_time_gives_same_displacement_at_different_rates() {
    let mut clock = GlobalClock::new();
    let conn = ConnectionId(7);
    let mut a = SimRoom::new(60.0, 1, clock.tx.subscribe(), conn);
    let mut b = SimRoom::new(15.0, 4, clock.tx.subscribe(), conn);

    let mut pos_a = None; // state at FROM_TICK
    let mut final_a = None;
    let mut pos_b = None;
    let mut final_b = None;
    let mut a_batch = 0u64;
    let mut b_batch = 0u64;

    for gt in 1..=TOTAL_TICKS {
        clock.tick();

        // Room A steps on every global tick: exactly one batch per tick
        // (v0 snapshot on the join tick, then one moved state per tick —
        // the target is never reached, so the entity is dirty every step).
        a_batch += 1;
        let (x, y) = observe(&next_batch(&mut a.out_rx).await);
        if a_batch == FROM_TICK {
            pos_a = Some((x, y));
        }
        if a_batch == TOTAL_TICKS {
            final_a = Some((x, y));
        }
        a.complete_join(conn);

        // Room B steps on every 4th global tick: one batch per step tick.
        // Batch k corresponds to global tick 4k.
        if gt % 4 == 0 {
            b_batch += 1;
            let (x, y) = observe(&next_batch(&mut b.out_rx).await);
            if 4 * b_batch == FROM_TICK {
                pos_b = Some((x, y));
            }
            if 4 * b_batch == TOTAL_TICKS {
                final_b = Some((x, y));
            }
            b.complete_join(conn);
        }
    }

    let (pos_a, final_a) = (pos_a.expect("A@300"), final_a.expect("A@600"));
    let (pos_b, final_b) = (pos_b.expect("B@300"), final_b.expect("B@600"));

    // Both windows are exactly 5.0 s of simulated time:
    //   A: 300 steps x (1/60) s      B: 75 steps x (4/60) s
    let da = (final_a.0 - pos_a.0, final_a.1 - pos_a.1);
    let db = (final_b.0 - pos_b.0, final_b.1 - pos_b.1);

    // Sanity: the entity really moved ~ speed * 5 s along the diagonal.
    assert!(
        da.0.abs() > 30.0 && da.1.abs() > 30.0,
        "A barely moved: {da:?}"
    );

    assert!(
        (da.0 - db.0).abs() < 0.1 && (da.1 - db.1).abs() < 0.1,
        "displacement differs by tick rate: 60 Hz {da:?} vs 15 Hz {db:?} \
         (same 5.0 s of sim time must cover the same distance)"
    );

    a.handle.abort();
    b.handle.abort();
}
