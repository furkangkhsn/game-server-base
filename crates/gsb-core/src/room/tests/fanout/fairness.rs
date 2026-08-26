//! Group fairness: every dirty group emits on the same tick, and a
//! full outbound channel drops that tick's batch and recovers.

use super::*;
use crate::room::tests::stubs::WarnCapture;

mod backpressure;

#[tokio::test]
async fn all_dirty_groups_emit_on_the_same_tick() {
    // The external-measurement scenario with contract-conforming
    // (per-group) bookkeeping: two per-connection groups whose
    // content is the whole world, and the world changes on every
    // tick. Every group must emit on every tick — the group visited
    // first by the room must not make the later groups see "no
    // change" (that is exactly what a ledger shared across groups
    // does; the `GameLogic::snapshot` contract forbids it).
    let (step_tx, mut steps) = mpsc::channel(64);
    let (tick_tx, tick_rx) = broadcast::channel(64);
    let (control, control_rx) = channel(128);
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(3),
            ..Default::default()
        }, // keep-alive 1 Hz at 30 Hz: never due in this test's window
        (),
        Box::new(FairLogic {
            last_world: 0,
            last_emitted: HashMap::new(),
            step_no: 0,
            steps: step_tx,
        }),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
            None,
    );
    let handle = tokio::spawn(actor.run());
    let t0 = Instant::now();
    let mut next_tick = 0;
    let mut tick = || {
        next_tick += 1;
        let at = t0 + Duration::from_secs_f64(next_tick as f64 / 30.0);
        tick_tx
            .send(TickInfo {
                tick: next_tick,
                at,
            })
            .expect("room subscriber alive");
    };

    // conn 1 joins (tick 1); the control is processed on the room's
    // next step, so the tick goes out before the reply is awaited.
    let (out1_tx, mut a_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply1_tx, reply1_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(1),
            out: out1_tx,
            reply: reply1_tx,
        })
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), reply1_rx)
        .await
        .expect("join reply timeout")
        .expect("join reply dropped")
        .expect("join accepted (room not full)");

    // conn 2 joins (tick 2).
    let (out2_tx, mut b_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply2_tx, reply2_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(2),
            out: out2_tx,
            reply: reply2_tx,
        })
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), reply2_rx)
        .await
        .expect("join reply timeout")
        .expect("join reply dropped")
        .expect("join accepted (room not full)");

    // 25-tick window: the world changes on every tick, so BOTH
    // groups are dirty on every tick.
    for _ in 0..25 {
        tick();
    }
    wait_steps(&mut steps, 27).await;

    let a_all = drain_all(&mut a_rx).await;
    let b_all = drain_all(&mut b_rx).await;
    // A: its join tick (world 1) + B's join tick (world 2) + all 25
    // window ticks. B: its join tick + all 25 window ticks.
    let seq = |batches: &Vec<Vec<FrameBody>>| {
        batches
            .iter()
            .map(|b| {
                u64::from_le_bytes(
                    b[0]
                        .payload
                        .get(0..8)
                        .expect("8-byte payload")
                        .try_into()
                        .expect("8-byte payload"),
                )
            })
            .collect::<Vec<u64>>()
    };
    let a_seq = seq(&a_all);
    let b_seq = seq(&b_all);
    assert_eq!(
        a_seq,
        (1..=27).collect::<Vec<_>>(),
        "A must emit on every tick the world changed"
    );
    assert_eq!(
        b_seq,
        (2..=27).collect::<Vec<_>>(),
        "B must emit on every tick the world changed (no starvation)"
    );

    control
        .send(RoomControl::Shutdown)
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not shut down")
        .expect("room task panicked");
}
