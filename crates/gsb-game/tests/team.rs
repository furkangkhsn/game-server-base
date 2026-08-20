//! Team fog of war through the real room actor (public API only).
//!
//! This is the spec's **cheat test**: it asserts the *content* of what each
//! connection's out channel actually receives (the per-connection batches),
//! not a bandwidth number. Information that is absent from a team's
//! package never reaches that team's clients — "the client does not have
//! it" is proven by the bytes that were (not) shipped.
//!
//! (The exact same-tick asymmetry — both teams' snapshots of ONE tick
//! compared — is pinned at the logic level in `gsb_game::team::tests`; the
//! actor level here proves the room's fan-out delivers each group's
//! package to exactly that group's members.)
//!
//! Teams come from the connection id (`conn % 2` — game state, not
//! position): conn 2 → team 0; conns 1 and 3 → team 1. Positions are
//! driven over the per-connection `MOVE_TO` channel and the test advances
//! the manual ticker until entities settle at their targets
//! (`DEFAULT_SPEED = 10 u/s` ⇒ ≤166 units over 500 ticks, which covers
//! every spawn-to-target distance used here).

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
use gsb_game::team::TeamRoom;
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
/// `tests/aoi.rs`), parameterized over the team-fog room.
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
            Box::new(TeamRoom::new(25.0)),
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

/// The cheat test through the actor: C (team 1, 40 from A) is in the
/// team-1 package (B's and C's channels) but its bytes NEVER reach A's
/// channel (team 0's package). B (10 from A) is in both. Then B leaves
/// and re-enters A's team's vision — the wire identity B carries in A's
/// package is the same across the whole transition (identity does not
/// change on a visibility transition).
#[tokio::test]
async fn team_fog_fanout_cheat_and_identity() {
    let mut room = TestRoom::new();

    let c_a = ConnectionId(2); // team 0 (even)
    let c_b = ConnectionId(1); // team 1 (odd)
    let c_c = ConnectionId(3); // team 1 (odd)
    let (a_id, mut a_rx, a_act) = room.join(c_a).await;
    let (b_id, mut b_rx, b_act) = room.join(c_b).await;
    let (c_id, mut c_rx, c_act) = room.join(c_c).await;
    assert_ne!(a_id, b_id);
    assert_ne!(b_id, c_id);
    assert_ne!(a_id, c_id);

    // A (0,0); B (10,0) — 10 < 25, in A's team's vision;
    // C (40,0) — 40 > 25, OUT of A's team's vision.
    move_to(&a_act, c_a, 0, 0).await;
    move_to(&b_act, c_b, 10, 0).await;
    move_to(&c_act, c_c, 40, 0).await;
    let (a_ids, b_ids, c_ids) = advance(&mut room, &mut a_rx, &mut b_rx, &mut c_rx, 500).await;

    // Steady state (the last snapshot each connection received):
    assert!(
        a_ids.contains(&a_id) && a_ids.contains(&b_id),
        "A sees A and B (B within 25): {a_ids:?}"
    );
    assert!(
        !a_ids.contains(&c_id),
        "C is out of A's team's vision: its bytes never reached A's channel: {a_ids:?}"
    );
    assert!(
        b_ids.contains(&a_id) && b_ids.contains(&b_id) && b_ids.contains(&c_id),
        "team 1 sees all three (A within 25 of B): {b_ids:?}"
    );
    assert!(
        c_ids.contains(&a_id) && c_ids.contains(&b_id) && c_ids.contains(&c_id),
        "team 1 sees all three: {c_ids:?}"
    );

    // B leaves A's team's vision ((60,60): 84.8 from A, 78 from B's spot).
    move_to(&b_act, c_b, 60, 60).await;
    let (a_ids2, _b_ids2, c_ids2) =
        advance(&mut room, &mut a_rx, &mut b_rx, &mut c_rx, 600).await;
    assert!(
        !a_ids2.contains(&b_id),
        "B dropped out of A's team's package: {a_ids2:?}"
    );
    assert!(
        a_ids2.contains(&a_id) && !a_ids2.contains(&c_id),
        "A's package is exactly its own team (B gone, C never was): {a_ids2:?}"
    );
    assert!(
        c_ids2.contains(&b_id),
        "B is still in its OWN team's package (own-team visibility is unconditional): {c_ids2:?}"
    );

    // B re-enters A's team's vision ((10,0) again): the wire identity B
    // carries in A's package must be the SAME one from before the
    // transition — previously untested, now pinned.
    move_to(&b_act, c_b, 10, 0).await;
    let (a_ids3, _b_ids3, _c_ids3) =
        advance(&mut room, &mut a_rx, &mut b_rx, &mut c_rx, 600).await;
    assert!(
        a_ids3.contains(&b_id),
        "B back in A's team's package — with the SAME wire id it had before \
         leaving (identity did not change on the visibility transition): {a_ids3:?}"
    );
    assert!(a_ids3.contains(&a_id) && !a_ids3.contains(&c_id));
}
