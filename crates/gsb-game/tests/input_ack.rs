//! Section A: input ordering + acknowledgments, end to end through the
//! real room actor (public API only, demo room).
//!
//! - the ack stream is MONOTONIC and never acknowledges more than the
//!   server has processed (per tick and in total);
//! - duplicate and reordered-late inputs are SILENTLY DROPPED (a normal
//!   race — re-sending one's own input is always legitimate), never
//!   regress the entity, and gaps never block the mark (high-water, not
//!   contiguity);
//! - a rejoin resets the input session on BOTH sides (the client
//!   restarts at seq 1; the server's stale high-water mark cannot eat
//!   the fresh session's first input);
//! - legacy unnumbered input (`seq = 0`) is processed but never acked
//!   (pre-seq clients keep working, unchanged).

use std::time::{Duration, Instant};

use bevy_ecs::prelude::World;
use bytes::Bytes;
use gsb_core::channel::{channel, FrameBatch, Mailbox};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::room::{Action, RoomActor, RoomConfig, RoomControl};
use gsb_core::ticker::TickInfo;
use gsb_game::game::{Private, WorldSnapshot};
use gsb_game::op;
use gsb_game::room::DemoRoom;
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
            Box::new(DemoRoom::new()),
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

    async fn leave(&mut self, conn: ConnectionId, entity: EntityId) {
        self.control
            .send(RoomControl::Leave { conn, entity })
            .await
            .expect("control channel alive");
        self.tick();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Send a NUMBERED input (`seq = 0` for the legacy path).
async fn send_seq(actions: &Mailbox<Action>, conn: ConnectionId, x: i32, y: i32, seq: u64) {
    let msg = gsb_game::game::MoveTo { x, y, seq };
    actions
        .send(Action {
            conn,
            op: op::MOVE_TO,
            payload: Bytes::from(msg.encode_to_vec()),
        })
        .await
        .expect("action channel alive");
}

/// Advance `ticks`, draining the connection's out channel after each
/// tick; collect every ack seen (and count the private frames per batch,
/// for the one-private-frame-per-tick rule) and track the latest group
/// snapshot's positions (`view`, the client half of the protocol — the
/// demo room's stream is full snapshots, so every frame replaces it).
async fn advance_and_collect(
    room: &mut TestRoom,
    rx: &mut mpsc::Receiver<FrameBatch>,
    ticks: u32,
    acks: &mut Vec<u64>,
    view: &mut std::collections::HashMap<u64, (i32, i32)>,
) {
    for _ in 0..ticks {
        room.tick();
        tokio::time::sleep(Duration::from_millis(1)).await;
        while let Ok(batch) = rx.try_recv() {
            let mut privates = 0;
            for f in batch.iter() {
                match f.op {
                    op::WORLD_SNAPSHOT => {
                        let s = WorldSnapshot::decode(f.payload.as_ref()).expect("decodable");
                        view.clear();
                        for e in &s.entities {
                            view.insert(e.entity, (e.x, e.y));
                        }
                    }
                    op::PRIVATE => {
                        privates += 1;
                        let p = Private::decode(f.payload.as_ref()).expect("decodable private");
                        match p.payload {
                            Some(gsb_game::game::private::Payload::Ack(a)) => {
                                acks.push(a.processed_up_to);
                            }
                            Some(gsb_game::game::private::Payload::Snapshot(_)) => {
                                panic!("the demo room sends no private snapshots");
                            }
                            None => panic!("empty private oneof"),
                        }
                    }
                    other => panic!("unexpected frame op {other}"),
                }
            }
            assert!(
                privates <= 1,
                "at most one private frame per tick per connection"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 1. The ack is monotonic and never exceeds what was processed.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ack_is_monotonic_and_never_exceeds_processed() {
    let mut room = TestRoom::new();
    let (a_id, mut a_rx, a_act) = room.join(ConnectionId(1)).await;

    let mut acks: Vec<u64> = Vec::new();
    let mut view: std::collections::HashMap<u64, (i32, i32)> = std::collections::HashMap::new();
    // Five numbered inputs, one per tick (the entity chases (k, 0)).
    for k in 1u64..=5 {
        send_seq(&a_act, ConnectionId(1), k as i32, 0, k).await;
        advance_and_collect(&mut room, &mut a_rx, 2, &mut acks, &mut view).await;
        // The invariant, checked as it goes: no ack may ever report a
        // sequence the server has not processed (it can't exceed the
        // highest sequence SENT, let alone the last processed).
        for a in &acks {
            assert!(*a <= k, "ack {a} exceeds the highest sent seq {k}");
        }
    }
    // Let the final ack land (the ack of seq k arrives on the tick after
    // seq k was processed).
    advance_and_collect(&mut room, &mut a_rx, 4, &mut acks, &mut view).await;

    // Monotonic (non-decreasing), and the final mark is exactly the
    // last processed sequence.
    assert!(!acks.is_empty(), "acks arrived: {acks:?}");
    for w in acks.windows(2) {
        assert!(w[1] >= w[0], "the ack stream is monotonic: {acks:?}");
    }
    assert_eq!(
        *acks.last().unwrap(),
        5,
        "the final ack is the high-water mark (all five processed): {acks:?}"
    );
    // And the entity actually followed the processed inputs (settle:
    // the spawn lattice is ±50, so the chase takes longer than the
    // ack-cadence window above).
    advance_and_collect(&mut room, &mut a_rx, 500, &mut acks, &mut view).await;
    assert_eq!(view.get(&a_id).copied(), Some((5, 0)), "the entity moved: {view:?}");
}

// ─────────────────────────────────────────────────────────────────────────
// 2. Duplicates are dropped (no regression), gaps don't block the mark.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn duplicates_dropped_gaps_dont_block_the_mark() {
    let mut room = TestRoom::new();
    let (a_id, mut a_rx, a_act) = room.join(ConnectionId(1)).await;

    let mut acks: Vec<u64> = Vec::new();
    let mut view: std::collections::HashMap<u64, (i32, i32)> = std::collections::HashMap::new();
    // seq 1 → (10,0), settle; seq 2 → (20,0), settle.
    send_seq(&a_act, ConnectionId(1), 10, 0, 1).await;
    advance_and_collect(&mut room, &mut a_rx, 60, &mut acks, &mut view).await;
    send_seq(&a_act, ConnectionId(1), 20, 0, 2).await;
    advance_and_collect(&mut room, &mut a_rx, 60, &mut acks, &mut view).await;

    // A DUPLICATE of seq 1 (re-sending one's own input — a normal
    // network race): it must be silently dropped (it is ≤ the high-water
    // mark) and must NOT re-target the entity at (10,0) — applying a
    // stale input would regress the entity.
    send_seq(&a_act, ConnectionId(1), 10, 0, 1).await;
    advance_and_collect(&mut room, &mut a_rx, 30, &mut acks, &mut view).await;

    // A GAP: seq 4 is never sent (it "was lost"); seq 5 must still mark
    // the high-water (gaps do not block the mark — the transport, not
    // the game, owns delivery).
    send_seq(&a_act, ConnectionId(1), 30, 0, 5).await;
    advance_and_collect(&mut room, &mut a_rx, 60, &mut acks, &mut view).await;
    advance_and_collect(&mut room, &mut a_rx, 4, &mut acks, &mut view).await;

    // The mark reached 5 (the duplicate did not count, the gap did not
    // block).
    assert_eq!(
        *acks.last().unwrap(),
        5,
        "the high-water mark is 5 (duplicate dropped, gap not blocking): {acks:?}"
    );
    // (Settle: the spawn lattice is ±50, so the chase takes longer than
    // the per-input windows above.)
    advance_and_collect(&mut room, &mut a_rx, 500, &mut acks, &mut view).await;
    // And the entity is at seq 5's target — the duplicate did not
    // regress it.
    assert_eq!(
        view.get(&a_id).copied(),
        Some((30, 0)),
        "the duplicate input did not regress the entity: {view:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// 3. A rejoin resets the input session on both sides.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rejoin_resets_the_input_session() {
    let mut room = TestRoom::new();
    let conn = ConnectionId(1);
    let (entity, mut rx, actions) = room.join(conn).await;

    // Session 1: seq 1..3 processed, acked.
    let mut acks: Vec<u64> = Vec::new();
    let mut view: std::collections::HashMap<u64, (i32, i32)> = std::collections::HashMap::new();
    for k in 1u64..=3 {
        send_seq(&actions, conn, k as i32, 0, k).await;
        advance_and_collect(&mut room, &mut rx, 2, &mut acks, &mut view).await;
    }
    advance_and_collect(&mut room, &mut rx, 4, &mut acks, &mut view).await;
    assert_eq!(*acks.last().unwrap(), 3, "session 1 fully acked: {acks:?}");

    // Leave, rejoin: a NEW session. If the server kept session 1's
    // high-water mark (3), the new session's seq 1 would be dropped as
    // "stale" forever — the server-side reset is what makes rejoin work.
    room.leave(conn, entity).await;
    let (entity2, mut rx2, actions2) = room.join(conn).await;
    assert_ne!(entity, entity2, "the rejoin mints a fresh entity");

    let mut acks2: Vec<u64> = Vec::new();
    let mut view2: std::collections::HashMap<u64, (i32, i32)> = std::collections::HashMap::new();
    send_seq(&actions2, conn, 7, 0, 1).await; // the fresh session starts at 1
    advance_and_collect(&mut room, &mut rx2, 40, &mut acks2, &mut view2).await;

    assert_eq!(
        *acks2.last().unwrap(),
        1,
        "the fresh session's seq 1 is processed and acked (the old mark \
         did not eat it): {acks2:?}"
    );
    // (Settle: the spawn lattice is ±50.)
    advance_and_collect(&mut room, &mut rx2, 500, &mut acks2, &mut view2).await;
    // The fresh entity moved to the fresh target (the new session is
    // live, not frozen).
    assert_eq!(
        view2.get(&entity2).copied(),
        Some((7, 0)),
        "the re-joined entity follows the new session's input: {view2:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// 4. Legacy unnumbered input is processed but never acked.
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn legacy_unnumbered_input_is_processed_but_never_acked() {
    let mut room = TestRoom::new();
    let (a_id, mut a_rx, a_act) = room.join(ConnectionId(1)).await;

    // A pre-seq client: unnumbered inputs only (seq = 0).
    let mut acks: Vec<u64> = Vec::new();
    let mut view: std::collections::HashMap<u64, (i32, i32)> = std::collections::HashMap::new();
    send_seq(&a_act, ConnectionId(1), 5, 0, 0).await;
    advance_and_collect(&mut room, &mut a_rx, 60, &mut acks, &mut view).await;
    send_seq(&a_act, ConnectionId(1), 9, 0, 0).await;
    advance_and_collect(&mut room, &mut a_rx, 60, &mut acks, &mut view).await;

    // (Settle: the spawn lattice is ±50.)
    advance_and_collect(&mut room, &mut a_rx, 500, &mut acks, &mut view).await;
    // The entity followed BOTH inputs (unnumbered input is processed).
    assert_eq!(view.get(&a_id).copied(), Some((9, 0)), "unnumbered input is processed: {view:?}");

    // But NO ack was ever sent (seq 0 never advances the high-water
    // mark; a pre-seq client sees exactly the old wire semantics).
    assert_eq!(acks, Vec::<u64>::new(), "no acks for a legacy (unnumbered) client");
}
