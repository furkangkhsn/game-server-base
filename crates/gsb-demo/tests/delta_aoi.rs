//! The cell-encoded delta AOI, end to end through the real room actor
//! (public API only) with a faithful CLIENT view: every frame is applied
//! exactly the way `game.proto` prescribes (a full replaces the view; a
//! delta applies on top in the fixed order `removed` → `cell_exits` →
//! `entities`; a delta with no accepted snapshot to apply against is
//! dropped until the next full; the one-shot private full is applied
//! unconditionally — the per-connection baseline reset).
//!
//! The tests lock in the spec's accuracy rules:
//! - the delta stream and the full stream converge to the same client
//!   view (two paths, one result);
//! - a cell-changing entity is seen correctly by clients that see both
//!   cells, only the source, or only the target — no ghosts, no
//!   duplicates, identity preserved;
//! - a cell that leaves a group's view makes its entities vanish
//!   client-side (one `CellExit` record per cell) — and the record names
//!   the cell by its INDEX, so a cell away from the origin forgets
//!   exactly its own entities;
//! - a mid-join client sees the full world (one-shot private full; the
//!   group stays in delta mode for everyone);
//! - a client that lost deltas recovers within the keep-alive bound
//!   (and never misapplies the deltas that arrive during the gap).
//!
//! `cell_size = 20`: Cell(i, j) spans `[20i, 20i+19]²` in wire
//! coordinates. Movement is 10 u/s at 30 Hz = 1/3 wire unit per tick;
//! entities settle (stop) exactly at their targets.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use bevy_ecs::prelude::World;
use bytes::Bytes;
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::room::{Action, RoomActor, RoomConfig, RoomControl};
use gsb_core::ticker::TickInfo;
use gsb_demo::aoi::AoiRoom;
use gsb_demo::game::{CellExit, EntityRecord, Private, WorldSnapshot};
use gsb_demo::op;
use gsb_demo::prelude::*;
use gsb_kit::client::{ClientDecoder, ClientError, ClientView, PrivateEvent};
use prost::Message;
use tokio::sync::{broadcast, mpsc, oneshot};

const WAIT: Duration = Duration::from_secs(5);
const PERIOD_SECS: f64 = 1.0 / 30.0;
/// The keep-alive period of the default room config (30 Hz tick, 1 Hz
/// keep-alive) — the delta-loss recovery bound this file measures.
const KEEPALIVE_EVERY: u64 = 30;

async fn reply<T>(rx: oneshot::Receiver<T>) -> T {
    tokio::time::timeout(WAIT, rx)
        .await
        .expect("timed out waiting for reply")
        .expect("reply dropped")
}

/// The demo's decode seam for the kit's reference client
/// (`gsb_kit::client`, the client rules of `kit.proto` — the load
/// generator's client view runs the same one): a record is kept as its
/// wire position, in the server's own cell of it (floor of the WIRE
/// coordinates / cell_size — the client needs it to service `CellExit`);
/// a `CellExit` names the cell by its index.
struct DemoDecoder {
    cell_size: f32,
}

impl ClientDecoder for DemoDecoder {
    type Record = (i32, i32);
    type Cell = (i32, i32);

    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32)), ClientError> {
        let e = EntityRecord::decode(body)?;
        Ok((e.entity, (e.x, e.y)))
    }

    fn cell_of(&self, &(x, y): &(i32, i32)) -> (i32, i32) {
        (
            (x as f32 / self.cell_size).floor() as i32,
            (y as f32 / self.cell_size).floor() as i32,
        )
    }

    fn cell_exit(&self, body: &[u8]) -> Result<(i32, i32), ClientError> {
        let c = CellExit::decode(body)?;
        Ok((c.x, c.y))
    }
}

/// The client-side world view (the protocol's client half).
type View = ClientView<DemoDecoder>;

/// The view as wire id → position.
fn entities(view: &View) -> HashMap<u64, (i32, i32)> {
    view.iter().map(|(id, &at)| (id, at)).collect()
}

/// The wire ids in the view.
fn ids(view: &View) -> BTreeSet<u64> {
    view.ids().collect()
}

/// One observed connection: its out channel + the view it applies every
/// batch to (which keeps the per-test counters).
struct Conn {
    rx: mpsc::Receiver<FrameBatch>,
    actions: Mailbox<Action>,
    view: View,
    /// The last group snapshot that carried `cell_exits` (raw, for the
    /// one-record-per-cell assertion).
    last_exit_snap: Option<WorldSnapshot>,
}

impl Conn {
    fn new(rx: mpsc::Receiver<FrameBatch>, actions: Mailbox<Action>, cell_size: f32) -> Self {
        Self {
            rx,
            actions,
            view: View::new(DemoDecoder { cell_size }),
            last_exit_snap: None,
        }
    }

    /// Fulls applied: group fulls and one-shot private fulls.
    fn fulls(&self) -> u64 {
        self.view.counters().fulls
    }

    /// Group deltas applied.
    fn deltas(&self) -> u64 {
        self.view.counters().deltas
    }

    /// Group deltas dropped without a baseline.
    fn gap_drops(&self) -> u64 {
        self.view.counters().gap_drops
    }

    /// One-shot private fulls applied.
    fn private_fulls(&self) -> u64 {
        self.view.counters().private_fulls
    }

    /// Apply one batch (the frames in arrival order: the group snapshot
    /// precedes the private frame — the core's fan-out order). `drop`
    /// simulates loss: the batch is discarded entirely.
    fn apply_batch(&mut self, batch: &FrameBatch, drop: bool) {
        if drop {
            return;
        }
        for f in batch.iter() {
            match f.op {
                op::WORLD_SNAPSHOT => {
                    if self.view.apply_snapshot(f.payload.as_ref()).is_err() {
                        panic!("undecodable group snapshot");
                    }
                    let s = WorldSnapshot::decode(f.payload.as_ref()).expect("decodable");
                    if !s.cell_exits.is_empty() {
                        self.last_exit_snap = Some(s);
                    }
                }
                op::PRIVATE => match self.view.apply_private(f.payload.as_ref()) {
                    // Input acks are not part of the view.
                    Ok(PrivateEvent::Full { .. } | PrivateEvent::Ack(_)) => {}
                    Ok(PrivateEvent::Empty) => panic!("empty private oneof"),
                    Err(ClientError::PrivateDelta) => {
                        panic!("a private snapshot must be a full")
                    }
                    Err(_) => panic!("undecodable private frame"),
                },
                other => panic!("unexpected frame op {other}"),
            }
        }
    }
}

/// A room actor driven by a manually fed global ticker (mirrors
/// `aoi.rs`/`late_join.rs`), with the AOI logic.
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
            None,
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

    fn now_tick(&self) -> u64 {
        self.next_tick
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
        let (entity, actions) = reply(reply_rx)
            .await
            .expect("join accepted (room not full)");
        (entity, out_rx, actions)
    }
}

async fn move_to(actions: &Mailbox<Action>, conn: ConnectionId, x: i32, y: i32) {
    let msg = gsb_demo::game::MoveTo { x, y, seq: 0 };
    actions
        .send(Action {
            player: gsb_core::PlayerId(conn.0),
            conn,
            op: op::MOVE_TO,
            payload: Bytes::from(msg.encode_to_vec()),
        })
        .await
        .expect("action channel alive");
}

/// Advance `ticks` steps, draining every connection's out channel after
/// each tick (so the bounded channel never backs up). Each connection
/// applies every batch to its view (the client half of the protocol).
async fn advance(room: &mut TestRoom, conns: &mut [Conn], ticks: u32) {
    for _ in 0..ticks {
        room.tick();
        // Yield so the room (same runtime) finishes this step's fan-out.
        tokio::time::sleep(Duration::from_millis(1)).await;
        for c in conns.iter_mut() {
            while let Ok(batch) = c.rx.try_recv() {
                c.apply_batch(&batch, false);
            }
        }
    }
}

/// The joiner's view must equal `want` (wire id → position).
fn assert_view(view: &View, want: &[(u64, i32, i32)], what: &str) {
    let got: HashMap<u64, (i32, i32)> = want.iter().map(|(w, x, y)| (*w, (*x, *y))).collect();
    assert_eq!(entities(view), got, "client view wrong: {what}");
}

// ─────────────────────────────────────────────────────────────────────────
// 1. The delta stream and the full stream converge to the same view.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delta_stream_converges_with_full_stream() {
    let mut room = TestRoom::new();
    let (a_id, a_rx, a_act) = room.join(ConnectionId(1)).await;
    let (b_id, b_rx, b_act) = room.join(ConnectionId(2)).await;
    let (c_id, c_rx, c_act) = room.join(ConnectionId(3)).await;
    let mut conns = vec![
        Conn::new(a_rx, a_act, 20.0),
        Conn::new(b_rx, b_act, 20.0),
        Conn::new(c_rx, c_act, 20.0),
    ];

    // A and B share Cell(0,0); C is far (Cell(2,0)) in its own group —
    // outside A/B's 3×3 (which spans x∈{-1..1}).
    move_to(&conns[0].actions, ConnectionId(1), 0, 0).await;
    move_to(&conns[1].actions, ConnectionId(2), 15, 0).await;
    move_to(&conns[2].actions, ConnectionId(3), 45, 0).await;
    advance(&mut room, &mut conns, 500).await;
    assert_view(&conns[0].view, &[(a_id, 0, 0), (b_id, 15, 0)], "A settled");

    // Now A moves within the shared group's view ((0,0) → (3,3), still
    // Cell(0,0)): the group's stream is a DELTA stream from here on.
    move_to(&conns[0].actions, ConnectionId(1), 3, 3).await;
    // 60 ticks: A arrives (~13 ticks) and the group goes silent; the
    // keep-alive full lands on the next multiple-of-30 tick (540) and
    // re-delivers the complete view on the full path.
    advance(&mut room, &mut conns, 60).await;

    // The delta-fed view converged to the true content.
    assert_view(
        &conns[0].view,
        &[(a_id, 3, 3), (b_id, 15, 0)],
        "A's delta view",
    );
    // The far client (its own group, untouched by A's move) converged
    // too.
    assert_view(&conns[2].view, &[(c_id, 45, 0)], "C's far view");
    // The premise: both modes actually ran. A applied the fresh-group
    // full at join and deltas afterwards; C's silent group keeps alive
    // with FRESH fulls at 1 Hz (the keep-alive path); B applied deltas.
    assert!(
        conns[0].deltas() > 0,
        "A applied deltas (the delta stream ran)"
    );
    assert!(conns[0].fulls() >= 1, "A started from a full");
    assert!(
        conns[2].fulls() >= 2,
        "C's silent group kept alive with fresh fulls"
    );
    assert!(conns[1].deltas() > 0, "B applied deltas too");
}

// ─────────────────────────────────────────────────────────────────────────
// 2. A cell-changing entity: both-cells / source-only / target-only
//    clients — no ghosts, no duplicates, identity preserved.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cell_change_all_client_positions_no_ghosts_no_duplicates() {
    let mut room = TestRoom::new();
    // P (the mover) in Cell(0,0); B in Cell(0,0) (sees BOTH cells — its
    // group's 3×3 spans x∈{-1..1}, and B is also the oracle: its view
    // holds P's authoritative wire position through the move); S in
    // Cell(-1,0) (sees the SOURCE cell only — its 3×3 spans x∈{-2..0});
    // T in Cell(2,0) (sees the TARGET cell only — its 3×3 spans x∈{1..3}).
    let (p_id, p_rx, p_act) = room.join(ConnectionId(1)).await;
    let (b_id, b_rx, b_act) = room.join(ConnectionId(2)).await;
    let (s_id, s_rx, s_act) = room.join(ConnectionId(3)).await;
    let (t_id, t_rx, t_act) = room.join(ConnectionId(4)).await;
    let mut conns = vec![
        Conn::new(p_rx, p_act, 20.0), // 0: P (the mover)
        Conn::new(b_rx, b_act, 20.0), // 1: B (sees both cells; oracle)
        Conn::new(s_rx, s_act, 20.0), // 2: S (source cell only)
        Conn::new(t_rx, t_act, 20.0), // 3: T (target cell only)
    ];

    move_to(&conns[0].actions, ConnectionId(1), 5, 5).await;
    move_to(&conns[1].actions, ConnectionId(2), 0, 0).await;
    move_to(&conns[2].actions, ConnectionId(3), -10, 5).await;
    move_to(&conns[3].actions, ConnectionId(4), 45, 5).await;
    advance(&mut room, &mut conns, 500).await;

    // The settled layout.
    assert_view(
        &conns[1].view,
        &[(p_id, 5, 5), (b_id, 0, 0), (s_id, -10, 5)],
        "B (both cells) settled",
    );
    assert_view(
        &conns[2].view,
        &[(p_id, 5, 5), (s_id, -10, 5), (b_id, 0, 0)],
        "S (source only) settled",
    );
    assert_view(&conns[3].view, &[(t_id, 45, 5)], "T (target only) settled");

    // P crosses from Cell(0,0) to Cell(1,0).
    move_to(&conns[0].actions, ConnectionId(1), 25, 5).await;
    let mut b_ever_lost_p = false;
    let mut s_leaked_after_departure = false;
    let mut t_preghosted = false;
    for _ in 0..120 {
        room.tick();
        tokio::time::sleep(Duration::from_millis(1)).await;
        for c in [1, 2, 3].iter() {
            while let Ok(batch) = conns[*c].rx.try_recv() {
                conns[*c].apply_batch(&batch, false);
            }
        }
        // Invariants, tick by tick (B is the oracle: it sees both cells,
        // so its copy of P is P's wire position):
        if let Some((px, _py)) = conns[1].view.get(p_id).copied() {
            // S (source only) must never see P once P's wire position
            // left the source cell (x >= 20): a re-ghost.
            if px >= 20 && conns[2].view.contains(p_id) {
                s_leaked_after_departure = true;
            }
            // T (target only) must never see P while P's wire position
            // is still in the source cell (x < 20): a pre-ghost.
            if px < 20 && conns[3].view.contains(p_id) {
                t_preghosted = true;
            }
        } else {
            // B (both cells) must NEVER lose P — not even for one tick
            // mid-move (a ghost window).
            b_ever_lost_p = true;
        }
    }

    assert!(!b_ever_lost_p, "B (both cells) must never lose P mid-move");
    assert!(
        !s_leaked_after_departure,
        "S (source only) must not re-ghost P after it left"
    );
    assert!(
        !t_preghosted,
        "T (target only) must not see P before it arrives"
    );

    // The settled end state (P arrived at (25,5) = Cell(1,0): 20 units
    // = 60 ticks, well inside the 120-tick window).
    assert_view(
        &conns[1].view,
        &[(p_id, 25, 5), (b_id, 0, 0), (s_id, -10, 5)],
        "B after the crossing",
    );
    assert_view(
        &conns[2].view,
        &[(s_id, -10, 5), (b_id, 0, 0)],
        "S after P left (no ghost)",
    );
    assert_view(
        &conns[3].view,
        &[(t_id, 45, 5), (p_id, 25, 5)],
        "T after P arrived",
    );

    // Identity: the SAME wire id, consistent, in every view that holds P.
    for v in [&conns[1].view, &conns[3].view] {
        assert_eq!(
            v.get(p_id).copied(),
            Some((25, 5)),
            "P's identity/position consistent"
        );
    }
    assert!(!conns[2].view.contains(p_id), "S no longer holds P");
}

// ─────────────────────────────────────────────────────────────────────────
// 3. A cell leaving a group's view: the entities vanish client-side,
//    one CellExit record per cell.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cell_exit_entities_vanish_client_side() {
    let mut room = TestRoom::new();
    // Five movers into Cell(0,0); the observer O in Cell(-1,0) (its
    // group's 3×3 spans x∈{-2..0}: it includes Cell(0,0), but NOT
    // Cell(1,0) — the movers' destination).
    let mut conns = Vec::new();
    let mut ids = Vec::new();
    for i in 0..5usize {
        let conn = ConnectionId(1 + i as u64);
        let (id, rx, actions) = room.join(conn).await;
        ids.push(id);
        conns.push(Conn::new(rx, actions, 20.0));
        move_to(&conns[i].actions, conn, i as i32, 0).await;
    }
    let o_conn = ConnectionId(6);
    let (o_id, o_rx, o_act) = room.join(o_conn).await;
    conns.push(Conn::new(o_rx, o_act, 20.0));
    let observer = conns.len() - 1;
    move_to(&conns[observer].actions, o_conn, -10, 0).await;

    advance(&mut room, &mut conns, 500).await;

    // The observer sees all five movers (plus itself).
    let mut want_o: Vec<(u64, i32, i32)> = ids
        .iter()
        .enumerate()
        .map(|(i, &w)| (w, i as i32, 0))
        .collect();
    want_o.push((o_id, -10, 0));
    assert_view(&conns[observer].view, &want_o, "observer before departure");

    // All five movers leave the cell at once (to Cell(1,0)).
    for (i, c) in conns.iter_mut().enumerate().take(5) {
        move_to(&c.actions, ConnectionId(1 + i as u64), 25, 0).await;
    }
    advance(&mut room, &mut conns, 150).await;

    // The observer's view: all five vanished client-side (via the
    // cell-exit path — not one record per entity).
    assert_view(
        &conns[observer].view,
        &[(o_id, -10, 0)],
        "observer after the cell emptied",
    );

    // The raw record: exactly ONE CellExit (0,0), and no entity records
    // for the departed movers in that packet.
    let snap = conns[observer]
        .last_exit_snap
        .clone()
        .expect("the observer's stream carried a cell exit");
    assert_eq!(
        snap.cell_exits.len(),
        1,
        "one record per exited cell: {snap:?}"
    );
    let exit = &snap.cell_exits[0];
    assert_eq!(
        (exit.x, exit.y),
        (0, 0),
        "the exited cell is named: {snap:?}"
    );
    for &w in &ids {
        assert!(
            snap.entities.iter().all(|e| e.entity != w),
            "the departed entities are not re-carried: {snap:?}"
        );
    }

    // The movers themselves converged in their new cell (Cell(1,0)):
    // each sees all five at their destination.
    let want_m: Vec<(u64, i32, i32)> = ids.iter().map(|&w| (w, 25, 0)).collect();
    for c in conns.iter().take(5) {
        assert_view(&c.view, &want_m, "movers in the new cell");
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 3b. A `CellExit` names the CELL (its index), not a position: a cell
//     away from the origin leaves exactly its own entities behind.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cell_exit_names_the_cell_index_away_from_the_origin() {
    let mut room = TestRoom::new();
    // Five movers into Cell(1,0); the observer O in Cell(0,0) (its
    // group's 3×3 spans x∈{-1..1}: it includes Cell(1,0), but NOT
    // Cell(2,0) — the movers' destination). O sits in the cell a
    // position-style reading of `CellExit(1,0)` would name (floor(1/20)
    // = 0), so a misread exit forgets O itself and keeps the movers.
    let mut conns = Vec::new();
    let mut ids = Vec::new();
    for i in 0..5usize {
        let conn = ConnectionId(1 + i as u64);
        let (id, rx, actions) = room.join(conn).await;
        ids.push(id);
        conns.push(Conn::new(rx, actions, 20.0));
        move_to(&conns[i].actions, conn, 20 + i as i32, 0).await;
    }
    let o_conn = ConnectionId(6);
    let (o_id, o_rx, o_act) = room.join(o_conn).await;
    conns.push(Conn::new(o_rx, o_act, 20.0));
    let observer = conns.len() - 1;
    move_to(&conns[observer].actions, o_conn, 5, 0).await;
    advance(&mut room, &mut conns, 500).await;
    let mut want_o: Vec<(u64, i32, i32)> = ids
        .iter()
        .enumerate()
        .map(|(i, &w)| (w, 20 + i as i32, 0))
        .collect();
    want_o.push((o_id, 5, 0));
    assert_view(&conns[observer].view, &want_o, "observer before departure");

    // All five leave Cell(1,0) for Cell(2,0). The view is checked on the
    // very tick the exit frame is applied — before a keep-alive full
    // could paper over a misapplied exit.
    conns[observer].last_exit_snap = None; // exits seen while settling
    for (i, c) in conns.iter_mut().enumerate().take(5) {
        move_to(&c.actions, ConnectionId(1 + i as u64), 45, 0).await;
    }
    let mut exit_checked = false;
    for _ in 0..150 {
        advance(&mut room, &mut conns, 1).await;
        let o = &mut conns[observer];
        assert!(o.view.contains(o_id), "O never loses itself");
        if let Some(snap) = o.last_exit_snap.take() {
            let exits: Vec<(i32, i32)> = snap.cell_exits.iter().map(|e| (e.x, e.y)).collect();
            assert_eq!(exits, [(1, 0)], "the exited cell is named: {snap:?}");
            assert_view(&o.view, &[(o_id, 5, 0)], "observer on the exit tick");
            exit_checked = true;
        }
    }
    assert!(exit_checked, "the observer's stream carried the cell exit");
}

// ─────────────────────────────────────────────────────────────────────────
// 4. A mid-join client sees the full world (one-shot private full; the
//    group stays in delta mode for everyone).
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn late_join_sees_full_world_one_shot() {
    let mut room = TestRoom::new();
    let (a_id, a_rx, a_act) = room.join(ConnectionId(1)).await;
    let (b_id, b_rx, b_act) = room.join(ConnectionId(2)).await;
    let mut ab = vec![Conn::new(a_rx, a_act, 20.0), Conn::new(b_rx, b_act, 20.0)];
    move_to(&ab[0].actions, ConnectionId(1), 0, 0).await;
    move_to(&ab[1].actions, ConnectionId(2), 15, 0).await;
    advance(&mut room, &mut ab, 500).await;
    assert_view(&ab[0].view, &[(a_id, 0, 0), (b_id, 15, 0)], "A settled");

    // C joins late. Its FIRST batch must establish a baseline: a FULL
    // (the group's own fresh full when its spawn group is born, or the
    // one-shot private full when the spawn group is already established
    // — a bare delta would leave a fresh client without anything to
    // apply against).
    let (c_id, c_rx, c_act) = room.join(ConnectionId(3)).await;
    let mut c = Conn::new(c_rx, c_act, 20.0);
    let mut conns;
    // Drain C's first batch (the join tick's fan-out; yield so the room
    // task — same runtime — finishes it).
    tokio::time::sleep(Duration::from_millis(2)).await;
    let first = c.rx.try_recv().expect("the join tick delivered a batch");
    let mut saw_full = false;
    for f in first.iter() {
        if f.op == op::WORLD_SNAPSHOT {
            let s = WorldSnapshot::decode(f.payload.as_ref()).expect("decodable");
            if !s.delta {
                saw_full = true;
            }
        } else if f.op == op::PRIVATE {
            let p = Private::decode(f.payload.as_ref()).expect("decodable");
            if let Some(gsb_demo::game::private::Payload::Snapshot(s)) = p.payload {
                assert!(!s.delta, "a private snapshot must be a full");
                saw_full = true;
            }
        }
    }
    assert!(
        saw_full,
        "the joiner's first batch carries a full (its group's fresh full \
         or the one-shot private full): {first:?}"
    );

    // C walks to the shared cell and the run continues.
    move_to(&c.actions, ConnectionId(3), 5, 0).await;
    conns = vec![ab.pop().unwrap(), ab.pop().unwrap(), c];
    // (conns: [b, a, c] — the order is irrelevant to the assertions)
    advance(&mut room, &mut conns, 500).await;

    // C now shares Cell(0,0) with A and B and saw the full world of the
    // group (the one-shot private full was the baseline, and the group
    // kept feeding it DELTAS — no per-connection group was created).
    assert_view(
        &conns[2].view,
        &[(a_id, 0, 0), (b_id, 15, 0), (c_id, 5, 0)],
        "late joiner's settled view",
    );
    assert!(
        conns[2].private_fulls() >= 1,
        "C received the one-shot private full(s)"
    );
    assert!(
        conns[2].deltas() > 0,
        "C applied deltas (the group stayed in delta mode)"
    );

    // Proof the group is in delta mode (not full mode) for C: a small
    // move by A is a DELTA that updates C's view.
    let deltas_before = conns[2].deltas();
    move_to(&conns[1].actions, ConnectionId(1), 4, 0).await;
    advance(&mut room, &mut conns, 60).await;
    assert_eq!(
        conns[2].view.get(a_id).copied(),
        Some((4, 0)),
        "C sees A's move"
    );
    assert!(
        conns[2].deltas() > deltas_before,
        "C's view update came from a delta (group still in delta mode)"
    );
    assert_view(
        &conns[1].view,
        &[(a_id, 4, 0), (b_id, 15, 0), (c_id, 5, 0)],
        "A after the delta move",
    );
}

// ─────────────────────────────────────────────────────────────────────────
// 5. Delta-loss recovery: a client that lost deltas drops the gaps
//    (no misapplication) and recovers on the keep-alive full, within
//    the keep-alive period.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delta_loss_recovers_within_keepalive_bound() {
    let mut room = TestRoom::new();
    let (a_id, a_rx, a_act) = room.join(ConnectionId(1)).await;
    let (b_id, b_rx, b_act) = room.join(ConnectionId(2)).await;
    let mut conns = vec![Conn::new(a_rx, a_act, 20.0), Conn::new(b_rx, b_act, 20.0)];
    move_to(&conns[0].actions, ConnectionId(1), 10, 10).await;
    move_to(&conns[1].actions, ConnectionId(2), 15, 0).await;
    advance(&mut room, &mut conns, 500).await;
    assert_view(&conns[0].view, &[(a_id, 10, 10), (b_id, 15, 0)], "settled");

    // Keep the group ACTIVE (A chases a circle of radius 8 around
    // (10, 10) — x,y ∈ [2, 18], so A's cell stays Cell(0,0): no group
    // crossing, no private fulls, just an active delta stream): a new
    // target every 15 ticks.
    let chase = |tick: u64| -> Option<(i32, i32)> {
        tick.is_multiple_of(15).then(|| {
            let angle = (tick as f64 / 15.0) * 1.7;
            (
                (10.0 + angle.cos() * 8.0) as i32,
                (10.0 + angle.sin() * 8.0) as i32,
            )
        })
    };

    // Active, no loss: 38 ticks (the view is current at the window end).
    for _ in 0..38 {
        if let Some((x, y)) = chase(room.now_tick() + 1) {
            move_to(&conns[0].actions, ConnectionId(1), x, y).await;
        }
        room.tick();
        tokio::time::sleep(Duration::from_millis(1)).await;
        for c in conns.iter_mut() {
            while let Ok(batch) = c.rx.try_recv() {
                c.apply_batch(&batch, false);
            }
        }
    }
    let view_before_loss = entities(&conns[0].view);
    let fulls_before_window = conns[0].fulls();
    let b_stats_before_window = (conns[1].fulls(), conns[1].deltas(), conns[1].gap_drops());

    // LOSS: 12 ticks of A's batches dropped client-side (B is fine).
    for _ in 0..12 {
        if let Some((x, y)) = chase(room.now_tick() + 1) {
            move_to(&conns[0].actions, ConnectionId(1), x, y).await;
        }
        room.tick();
        tokio::time::sleep(Duration::from_millis(1)).await;
        while let Ok(batch) = conns[0].rx.try_recv() {
            conns[0].apply_batch(&batch, true); // dropped (loss)
        }
        while let Ok(batch) = conns[1].rx.try_recv() {
            conns[1].apply_batch(&batch, false);
        }
    }
    let loss_end = room.now_tick();

    // Delivery resumes: A's deltas arrive with a sequence gap → DROPPED
    // (not applied — no misapplication); the keep-alive full on the
    // next multiple-of-30 tick heals the view.
    let mut recovered_at: Option<u64> = None;
    for _ in 0..KEEPALIVE_EVERY * 2 {
        if let Some((x, y)) = chase(room.now_tick() + 1) {
            move_to(&conns[0].actions, ConnectionId(1), x, y).await;
        }
        room.tick();
        tokio::time::sleep(Duration::from_millis(1)).await;
        for c in conns.iter_mut() {
            while let Ok(batch) = c.rx.try_recv() {
                c.apply_batch(&batch, false);
            }
        }
        if recovered_at.is_none() && entities(&conns[0].view) != view_before_loss {
            recovered_at = Some(room.now_tick());
        }
        // (No early break: the window must run to the end so the
        // keep-alive full — the convergence guarantee — is observed.)
    }

    // The bound: recovery within one keep-alive period of the loss end.
    // (Best-effort application may heal it earlier — the first delivered
    // delta carries absolute positions — but the keep-alive full is the
    // GUARANTEE: it must have arrived inside the bound.)
    let recovered_at = recovered_at.expect("the view recovered");
    assert!(
        recovered_at <= loss_end + KEEPALIVE_EVERY + 1,
        "recovery at tick {recovered_at} exceeds the keep-alive bound \
         (loss ended {loss_end}, period {KEEPALIVE_EVERY})"
    );
    // The convergence guarantee actually ran: a FRESH full reached A
    // after the loss window (not a re-send — the view healed).
    assert!(
        conns[0].fulls() > fulls_before_window,
        "a keep-alive full healed A (fulls {} -> {})",
        fulls_before_window,
        conns[0].fulls()
    );
    // The healed view holds both entities.
    let ids = ids(&conns[0].view);
    assert!(
        ids.contains(&a_id) && ids.contains(&b_id),
        "healed view: {:?}",
        entities(&conns[0].view)
    );
    // B (no loss) never sat without a baseline in the window (its group
    // stream kept running for it; the keep-alive fulls it shares with A
    // are normal group traffic, not a sign of loss).
    assert_eq!(
        conns[1].gap_drops(),
        b_stats_before_window.2,
        "B was never without a baseline: {:?}",
        (conns[1].fulls(), conns[1].deltas(), conns[1].gap_drops())
    );
}

// ─────────────────────────────────────────────────────────────────────────
// 6. The packet is self-describing: the mode is on the wire (a
//    wrong-mode client detects it instead of silently misapplying).
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn packet_mode_is_self_describing_on_the_wire() {
    let mut room = TestRoom::new();
    let (a_id, a_rx, a_act) = room.join(ConnectionId(1)).await;
    let (b_id, b_rx, b_act) = room.join(ConnectionId(2)).await;
    let mut conns = vec![Conn::new(a_rx, a_act, 20.0), Conn::new(b_rx, b_act, 20.0)];
    move_to(&conns[0].actions, ConnectionId(1), 0, 0).await;
    move_to(&conns[1].actions, ConnectionId(2), 15, 0).await;
    advance(&mut room, &mut conns, 500).await;

    // A moves: the group's stream is deltas from here on; the
    // keep-alive tick 510 (500 is a multiple of 30, so the next one is
    // 510) ships the fresh full while A is still moving — both modes
    // appear on the wire in the same 15-tick window.
    move_to(&conns[0].actions, ConnectionId(1), 3, 0).await;
    let mut saw_full = false;
    let mut saw_delta = false;
    for _ in 0..15 {
        room.tick();
        tokio::time::sleep(Duration::from_millis(1)).await;
        for c in conns.iter_mut() {
            while let Ok(batch) = c.rx.try_recv() {
                for f in batch.iter() {
                    if f.op == op::WORLD_SNAPSHOT {
                        let s = WorldSnapshot::decode(f.payload.as_ref()).expect("decodable");
                        if s.delta {
                            saw_delta = true;
                        } else {
                            saw_full = true;
                        }
                        assert!(s.sequence > 0, "the sequence is always present");
                    }
                }
            }
        }
    }
    assert!(
        saw_delta,
        "an active group's stream is marked delta on the wire"
    );
    assert!(
        saw_full,
        "the keep-alive full is marked full on the wire (mode is \
         self-describing; a wrong-mode client can detect it)"
    );
    let _ = (a_id, b_id);
}
