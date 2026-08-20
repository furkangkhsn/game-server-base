//! Per-map-segment PVS through the real room actor (public API only).
//!
//! Pins what the *room* does with the sector group key (the logic-level
//! visibility-table behavior — the 3-unit wall test, linked sightlines,
//! OUT containment — lives in `gsb_game::pvs::tests`):
//!
//! - **the wall**: A in sector A at (-1, 0) and B in sector B at (2, 0)
//!   are 3 units apart and do NOT see each other (A and B are unlinked in
//!   the static visibility table — a distance-based AOI with radius >= 3
//!   would let them see each other and cannot pass this test);
//! - **the passage**: A and C (sector C at (-20, 25), linked with A) DO
//!   see each other, and B sees neither (B is linked only with D);
//! - **sector transition**: A moving into sector C keeps its wire identity
//!   and its new package is sector C's.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use bevy_ecs::prelude::World;
use bytes::Bytes;
use gsb_core::channel::{channel, FrameBatch, Mailbox};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::room::{Action, RoomActor, RoomConfig, RoomControl};
use gsb_core::ticker::TickInfo;
use gsb_game::game::WorldSnapshot;
use gsb_game::op;
use gsb_game::pvs::SectorRoom;
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
/// `tests/aoi.rs`), parameterized over the PVS room.
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
            Box::new(SectorRoom::new()),
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
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), gsb_core::error::CoreError>>();
        self.control
            .send(RoomControl::Join {
                conn,
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control channel alive");
        self.tick();
        let (entity, actions) = reply(reply_rx).await.expect("join accepted (room not full)");
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

/// Advance `ticks` steps, draining each connection's out channel after
/// every tick (so the bounded channel never backs up) and remembering the
/// latest WORLD_SNAPSHOT wire-id set seen per connection.
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

/// The wall + the passage, through the real fan-out: A and B (3 units
/// apart, unlinked sectors) never see each other; A and C (linked sectors)
/// see each other; B sees only itself. Then A crosses into C's sector:
/// identity unchanged, package now sector C's, and the wall did not move
/// (B still sees neither).
#[tokio::test]
async fn pvs_fanout_wall_and_sector_transition() {
    let mut room = TestRoom::new();

    let (a_id, mut a_rx, a_act) = room.join(ConnectionId(1)).await;
    let (b_id, mut b_rx, b_act) = room.join(ConnectionId(2)).await;
    let (c_id, mut c_rx, c_act) = room.join(ConnectionId(3)).await;
    assert_ne!(a_id, b_id);
    assert_ne!(b_id, c_id);
    assert_ne!(a_id, c_id);

    // A (-1, 0): sector A. B (2, 0): sector B (3 units from A, WALL
    // between them). C (-20, 25): sector C (x <= -10 in the north band;
    // linked with A, NOT with B).
    move_to(&a_act, ConnectionId(1), -1, 0).await;
    move_to(&b_act, ConnectionId(2), 2, 0).await;
    move_to(&c_act, ConnectionId(3), -20, 25).await;
    let (a_ids, b_ids, c_ids) = advance(&mut room, &mut a_rx, &mut b_rx, &mut c_rx, 500).await;

    // The wall: A and B are 3 units apart and invisible to each other
    // (unlinked in the static visibility table — a distance-AOI with
    // radius >= 3 could not produce these packages).
    assert!(
        a_ids.contains(&a_id) && !a_ids.contains(&b_id),
        "A sees itself, NOT B (wall): {a_ids:?}"
    );
    assert!(
        b_ids.contains(&b_id) && !b_ids.contains(&a_id),
        "B sees itself, NOT A (wall): {b_ids:?}"
    );

    // The passage: A and C are linked and see each other.
    assert!(
        a_ids.contains(&c_id),
        "A sees C (linked sectors): {a_ids:?}"
    );
    assert!(
        c_ids.contains(&a_id) && c_ids.contains(&c_id),
        "C sees A and itself: {c_ids:?}"
    );
    // B sees nothing beyond itself (B is linked only with D, which is
    // empty; A and C are unlinked with B).
    assert_eq!(b_ids, [b_id].into_iter().collect(), "B sees exactly itself: {b_ids:?}");
    assert!(!c_ids.contains(&b_id), "C does not see B: {c_ids:?}");

    // A crosses into sector C ((-15, 25), 5 units from C): wire identity
    // unchanged; A's package becomes sector C's (A and C now share the
    // group); the wall did not move: B's package is untouched.
    move_to(&a_act, ConnectionId(1), -15, 25).await;
    let (a_ids2, b_ids2, c_ids2) =
        advance(&mut room, &mut a_rx, &mut b_rx, &mut c_rx, 400).await;
    assert!(
        a_ids2.contains(&a_id) && a_ids2.contains(&c_id),
        "A (now in sector C) sees C and itself: {a_ids2:?}"
    );
    assert_eq!(
        a_ids2, c_ids2,
        "A and C now share sector C's package (same group ⇒ same bytes): {a_ids2:?} vs {c_ids2:?}"
    );
    assert_eq!(
        b_ids2,
        [b_id].into_iter().collect(),
        "B's package unchanged (the wall did not move): {b_ids2:?}"
    );
    // Identity across the sector crossing: A's wire id is the one it had
    // before the move (never re-minted).
    assert!(a_ids2.contains(&a_id), "A keeps its wire id: {a_ids2:?}");
}
