//! The AOI room through the real room actor (public API only), observed
//! with a faithful CLIENT view (the protocol's client half, per
//! `game.proto`): fulls replace the view, deltas apply on top. The old
//! "latest snapshot's wire-id set" observation no longer suffices — the
//! spatial stream is delta-coded (a group's packet is the union of its
//! cells' per-tick pieces, and silence writes no bytes) — so what a
//! client HOLDS is the observable, not what a frame happened to carry.
//!
//! Locked in here: same-cell co-residents are visible, the far cell is
//! not (the 3×3 neighborhood), and a cell transition moves the entity
//! between views WITHOUT changing its wire identity (the identity
//! invariant). The deeper delta-stream properties (no ghosts across all
//! client positions, cell-exit vanishing, late-join one-shot full,
//! loss-recovery bound) live in `delta_aoi.rs`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bevy_ecs::prelude::World;
use bytes::Bytes;
use gsb_core::channel::{channel, FrameBatch, Mailbox};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::room::{Action, RoomActor, RoomConfig, RoomControl};
use gsb_core::ticker::TickInfo;
use gsb_game::aoi::AoiRoom;
use gsb_game::game::{Private, WorldSnapshot};
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

/// The client-side world view (the protocol's client half — the same
/// rules as `View` in `delta_aoi.rs`; the spatial stream is
/// delta-coded, so the held view is the observable).
struct View {
    entities: HashMap<u64, (i32, i32)>,
    last_seq: Option<u64>,
}

impl View {
    fn new() -> Self {
        Self {
            entities: HashMap::new(),
            last_seq: None,
        }
    }

    fn apply_group(&mut self, s: &WorldSnapshot) {
        if s.sequence <= self.last_seq.unwrap_or(0) {
            return; // duplicate/stale
        }
        if s.delta {
            if self.last_seq.is_none() {
                return; // no baseline: drop until the next full
            }
            for &w in &s.removed {
                self.entities.remove(&w);
            }
            for e in &s.entities {
                self.entities.insert(e.entity, (e.x, e.y));
            }
        } else {
            self.entities.clear();
            for e in &s.entities {
                self.entities.insert(e.entity, (e.x, e.y));
            }
        }
        self.last_seq = Some(s.sequence);
    }

    fn apply_private_full(&mut self, s: &WorldSnapshot) {
        self.entities.clear();
        for e in &s.entities {
            self.entities.insert(e.entity, (e.x, e.y));
        }
        self.last_seq = Some(s.sequence);
    }

    fn ids(&self) -> Vec<u64> {
        self.entities.keys().copied().collect()
    }
}

struct Conn {
    rx: mpsc::Receiver<FrameBatch>,
    actions: Mailbox<Action>,
    view: View,
}

impl Conn {
    fn new(rx: mpsc::Receiver<FrameBatch>, actions: Mailbox<Action>) -> Self {
        Self {
            rx,
            actions,
            view: View::new(),
        }
    }

    fn apply_batch(&mut self, batch: &FrameBatch) {
        for f in batch.iter() {
            match f.op {
                op::WORLD_SNAPSHOT => {
                    let s = WorldSnapshot::decode(f.payload.as_ref()).expect("decodable");
                    self.view.apply_group(&s);
                }
                op::PRIVATE => {
                    let p = Private::decode(f.payload.as_ref()).expect("decodable private");
                    if let Some(gsb_game::game::private::Payload::Snapshot(s)) = p.payload {
                        self.view.apply_private_full(&s);
                    }
                }
                other => panic!("unexpected frame op {other}"),
            }
        }
    }
}

/// A room actor driven by a manually fed global ticker (mirrors the
/// historical AOI harness), with the AOI logic.
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
    let msg = gsb_game::game::MoveTo { x, y, seq: 0 };
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
/// every tick (so the bounded channel never backs up); every batch is
/// applied to that connection's client view.
async fn advance(room: &mut TestRoom, conns: &mut [Conn], ticks: u32) {
    for _ in 0..ticks {
        room.tick();
        // Yield so the room (same runtime) finishes this step's fan-out.
        tokio::time::sleep(Duration::from_millis(1)).await;
        for c in conns.iter_mut() {
            while let Ok(batch) = c.rx.try_recv() {
                c.apply_batch(&batch);
            }
        }
    }
}

#[tokio::test]
async fn aoi_room_fanout_and_cell_transition() {
    let mut room = TestRoom::new();

    let c_a = ConnectionId(1);
    let c_b = ConnectionId(2);
    let c_c = ConnectionId(3);
    let (a_id, a_rx, a_act) = room.join(c_a).await;
    let (b_id, b_rx, b_act) = room.join(c_b).await;
    let (c_id, c_rx, c_act) = room.join(c_c).await;
    assert_ne!(a_id, b_id);
    assert_ne!(b_id, c_id);
    assert_ne!(a_id, c_id);

    // A (0,0) and B (15,0) share Cell(0,0); C (45,0) is in the far cell
    // Cell(2,0) — outside each other's 3×3 blocks (cell_size 20).
    let mut conns = vec![
        Conn::new(a_rx, a_act),
        Conn::new(b_rx, b_act),
        Conn::new(c_rx, c_act),
    ];
    move_to(&conns[0].actions, c_a, 0, 0).await;
    move_to(&conns[1].actions, c_b, 15, 0).await;
    move_to(&conns[2].actions, c_c, 45, 0).await;
    advance(&mut room, &mut conns, 500).await;

    // Same-cell co-residents are visible; the far cell is not (the
    // clients' HELD views — the delta stream's observable).
    let a_ids = conns[0].view.ids();
    let b_ids = conns[1].view.ids();
    let c_ids = conns[2].view.ids();
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
    move_to(&conns[0].actions, c_a, 45, 0).await;
    advance(&mut room, &mut conns, 300).await;

    // A is now co-resident with C (each sees the other), and B — still in
    // Cell(0,0) — no longer sees A (the entity exit was applied). A's
    // wire identity is unchanged across the cell move (the identity
    // invariant).
    let a_ids2 = conns[0].view.ids();
    let b_ids2 = conns[1].view.ids();
    let c_ids2 = conns[2].view.ids();
    assert!(
        a_ids2.contains(&a_id) && a_ids2.contains(&c_id),
        "A now sees C: {a_ids2:?}"
    );
    assert!(
        c_ids2.contains(&c_id) && c_ids2.contains(&a_id),
        "C now sees A: {c_ids2:?}"
    );
    assert!(!b_ids2.contains(&a_id), "B no longer sees A after it left: {b_ids2:?}");
    assert!(
        a_ids2.contains(&a_id),
        "A keeps its wire id across the cell move: {a_ids2:?}"
    );
}
