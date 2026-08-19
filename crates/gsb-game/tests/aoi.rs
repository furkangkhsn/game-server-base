//! AOI behaviour through the real room actor (public API only).
//!
//! The logic-level invariants (visibility set, cell transition at the content
//! level, late-join block, broadcast set, "no change") are tested inline in
//! `gsb_game::aoi` (they need the room's private bookkeeping). This file locks
//! in what the *room* does with the spatial group key: `group_of` is
//! re-evaluated every tick (a player crossing a cell boundary changes group),
//! each connection receives exactly its own cell's block (near co-residents
//! present, far entities absent), and a player who moves cells starts
//! receiving the new cell's block.
//!
//! Positions are driven over the per-connection `MOVE_TO` channel and the test
//! advances the manual ticker until entities settle at their targets
//! (`DEFAULT_SPEED = 10 u/s` ⇒ ≤166 units over 500 ticks, which covers every
//! spawn-to-target distance in the 100×100 arena).

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use bevy_ecs::prelude::World;
use bytes::Bytes;
use gsb_core::channel::{channel, FrameBatch, Mailbox};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::room::{Action, RoomActor, RoomConfig, RoomControl};
use gsb_core::ticker::TickInfo;
use gsb_game::aoi::AoiRoom;
use gsb_game::game::WorldSnapshot;
use gsb_game::op;
use prost::Message;
use tokio::sync::{broadcast, mpsc, oneshot};

const WAIT: Duration = Duration::from_secs(5);
const PERIOD_SECS: f64 = 1.0 / 30.0;

async fn reply<T>(rx: oneshot::Receiver<T>) -> T {
    tokio::time::timeout(WAIT, rx)
        .await
        .expect("timed out waiting for reply")
        .expect("reply dropped")
}

/// The wire-id set carried by a batch's most recent WORLD_SNAPSHOT frame
/// (at most one per group per tick).
fn snap_ids(batch: &FrameBatch) -> Option<BTreeSet<u64>> {
    for f in batch.iter() {
        if f.op == op::WORLD_SNAPSHOT {
            return WorldSnapshot::decode(f.payload.as_ref())
                .ok()
                .map(|s| s.entities.iter().map(|e| e.entity).collect());
        }
    }
    None
}

/// A room actor driven by a manually fed global ticker (mirrors
/// `late_join.rs`), parameterized over the AOI group key (`AoiRoom` ⇒
/// `RoomActor<World, Cell>`).
struct TestRoom {
    tick_tx: broadcast::Sender<TickInfo>,
    control: Mailbox<RoomControl>,
    #[allow(dead_code)]
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
        let (metrics_tx, _metrics_rx) = mpsc::channel::<gsb_core::metrics::MetricsEvent>(1);
        let actor = RoomActor::new(
            config,
            World::new(),
            Box::new(AoiRoom::new(20.0)),
            tick_rx,
            control_rx,
            1, // room rate == global rate
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
}

async fn move_to(actions: &Mailbox<Action>, conn: ConnectionId, x: i32, y: i32) {
    let msg = gsb_game::game::MoveTo { x, y };
    actions
        .send(Action {
            conn,
            op: op::MOVE_TO,
            payload: Bytes::from(msg.encode_to_vec()),
        })
        .await
        .expect("action channel alive");
}

/// Advance `ticks` steps, draining each connection's out channel after every
/// tick (so the bounded channel never backs up) and remembering the latest
/// WORLD_SNAPSHOT wire-id set seen per connection.
async fn advance(
    room: &mut TestRoom,
    a: &mut mpsc::Receiver<FrameBatch>,
    b: &mut mpsc::Receiver<FrameBatch>,
    c: &mut mpsc::Receiver<FrameBatch>,
    ticks: u32,
) -> (BTreeSet<u64>, BTreeSet<u64>, BTreeSet<u64>) {
    let mut la = BTreeSet::new();
    let mut lb = BTreeSet::new();
    let mut lc = BTreeSet::new();
    for _ in 0..ticks {
        room.tick();
        // Yield so the room (same runtime) finishes this step's fan-out.
        tokio::time::sleep(Duration::from_millis(1)).await;
        // Drain each connection's out channel (so the bounded channel never
        // backs up), remembering the latest WORLD_SNAPSHOT per connection.
        while let Ok(batch) = a.try_recv() {
            if let Some(ids) = snap_ids(&batch) {
                la = ids;
            }
        }
        while let Ok(batch) = b.try_recv() {
            if let Some(ids) = snap_ids(&batch) {
                lb = ids;
            }
        }
        while let Ok(batch) = c.try_recv() {
            if let Some(ids) = snap_ids(&batch) {
                lc = ids;
            }
        }
    }
    (la, lb, lc)
}

#[tokio::test]
async fn aoi_room_fanout_and_cell_transition() {
    let mut room = TestRoom::new();

    let c_a = ConnectionId(1);
    let c_b = ConnectionId(2);
    let c_c = ConnectionId(3);
    let (a_id, mut a_rx, a_act) = room.join(c_a).await;
    let (b_id, mut b_rx, b_act) = room.join(c_b).await;
    let (c_id, mut c_rx, c_act) = room.join(c_c).await;
    assert_ne!(a_id, b_id);
    assert_ne!(b_id, c_id);
    assert_ne!(a_id, c_id);

    // Place A (0,0) and B (15,0) in the same cell Cell(0,0); C (45,0) in the
    // far cell Cell(2,0) — outside each other's 3×3 blocks (cell_size 20).
    move_to(&a_act, c_a, 0, 0).await;
    move_to(&b_act, c_b, 15, 0).await;
    move_to(&c_act, c_c, 45, 0).await;
    let (a_ids, b_ids, c_ids) = advance(&mut room, &mut a_rx, &mut b_rx, &mut c_rx, 500).await;

    // Same-cell co-residents are visible; the far cell is not.
    assert!(a_ids.contains(&a_id) && a_ids.contains(&b_id), "A sees B: {a_ids:?}");
    assert!(!a_ids.contains(&c_id), "A does not see far C: {a_ids:?}");
    assert!(b_ids.contains(&b_id) && b_ids.contains(&a_id), "B sees A: {b_ids:?}");
    assert!(!b_ids.contains(&c_id), "B does not see far C: {b_ids:?}");
    assert!(c_ids.contains(&c_id), "C sees itself: {c_ids:?}");
    assert!(
        !c_ids.contains(&a_id) && !c_ids.contains(&b_id),
        "far C sees only itself: {c_ids:?}"
    );

    // Cell transition: A moves into C's cell (45,0).
    move_to(&a_act, c_a, 45, 0).await;
    let (a_ids2, b_ids2, c_ids2) = advance(&mut room, &mut a_rx, &mut b_rx, &mut c_rx, 300).await;

    // A is now co-resident with C (each sees the other), and B — still in
    // Cell(0,0) — no longer sees A. A's wire identity is unchanged across the
    // cell move (the identity invariant).
    assert!(a_ids2.contains(&a_id) && a_ids2.contains(&c_id), "A now sees C: {a_ids2:?}");
    assert!(c_ids2.contains(&c_id) && c_ids2.contains(&a_id), "C now sees A: {c_ids2:?}");
    assert!(!b_ids2.contains(&a_id), "B no longer sees A after it left: {b_ids2:?}");
    assert!(
        a_ids2.contains(&a_id),
        "A keeps its wire id across the cell move: {a_ids2:?}"
    );
}
