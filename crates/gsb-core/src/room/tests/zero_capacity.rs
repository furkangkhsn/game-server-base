//! F21: a zero action capacity (`conn_action = 0`, flat or per room)
//! reached `mpsc::channel(0)` at the room's first join (and on every
//! resume) and panicked the room actor. A zero-capacity action channel
//! has no meaning: it is one slot, as the control channel always was
//! (`crate::channel::channel`) — and the room keeps ingesting.

use super::binding::RebindLogic;
use super::*;

fn zero() -> RoomConfig {
    RoomConfig {
        id: RoomId(1),
        action_capacity: 0,
        ..Default::default()
    }
}

/// The join path: one slot, and the action in it is ingested.
#[tokio::test]
async fn a_zero_capacity_join_gets_one_slot() {
    let (dt_tx, _dts) = mpsc::channel(16);
    let (op_tx, mut ops) = mpsc::channel(16);
    let mut h = Harness::new(
        1,
        zero(),
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let (_e, actions) = h.join(ConnectionId(1), 1).await;
    assert_eq!(actions.max_capacity(), 1, "one slot");
    actions
        .send(Action {
            conn: ConnectionId(1),
            player: PlayerId(1),
            op: 0x1500,
            payload: bytes::Bytes::new(),
        })
        .await
        .expect("the room is alive");
    h.tick(Duration::from_secs_f64(1.0 / 30.0));
    let op = tokio::time::timeout(Duration::from_secs(2), ops.recv())
        .await
        .expect("ingested in time")
        .expect("ops open");
    assert_eq!(op, 0x1500);
    h.shutdown().await;
}

/// The resume path builds its own fresh channel: one slot as well.
#[test]
fn a_zero_capacity_resume_gets_one_slot() {
    let (_tick_tx, tick_rx) = broadcast::channel(4);
    let (_control, control_rx) = channel(16);
    let mut actor = RoomActor::new(
        zero(),
        (),
        Box::new(RebindLogic::new()),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    let (out_tx, _o) = mpsc::channel::<FrameBatch>(2);
    let (rtx, mut rrx) = oneshot::channel();
    actor.handle_control(RoomControl::Join {
        conn: ConnectionId(1),
        out: out_tx,
        reply: rtx,
    });
    let (entity, _a) = rrx.try_recv().expect("sent").expect("joined");
    actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity,
        identity: "ana".into(),
    });
    let (out_tx, _o) = mpsc::channel::<FrameBatch>(2);
    let (rtx, mut rrx) = oneshot::channel();
    actor.handle_control(RoomControl::Resume {
        conn: ConnectionId(2),
        epoch: 1,
        identity: "ana".into(),
        out: out_tx,
        reply: rtx,
        claims: None,
    });
    let (again, actions) = rrx.try_recv().expect("sent").expect("resumed");
    assert_eq!(again, entity, "the parked row came back");
    assert_eq!(actions.max_capacity(), 1, "one slot");
}
