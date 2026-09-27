//! The room's rule on the shard: a request still unread in the channel
//! when the ceiling's default action leaves a PARK behind is counted as
//! the released channel goes (`requests_dropped_unread`), like a
//! despawn's.

use super::*;
use crate::channel::Mailbox;
use crate::room::Action;

/// Join `conn` keeping its action channel's sending half.
fn join_with_actions(
    a: &mut ShardActor<TWorld, (), TState, TStrip>,
    conn: ConnectionId,
) -> (mpsc::Receiver<FrameBatch>, Mailbox<Action>) {
    let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply_tx, mut reply_rx) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn,
            epoch: 1,
            identity: "ana".to_string(),
            out: out_tx,
            reply: reply_tx,
        },
        1,
    ));
    let (_entity, actions) = reply_rx
        .try_recv()
        .expect("join reply is synchronous")
        .expect("join accepted");
    (out_rx, actions)
}

#[tokio::test]
async fn a_request_unread_when_a_sharded_park_is_left_behind_is_counted() {
    let hold = Detach::Hold {
        grace: Some(Duration::from_secs(600)),
        to: ExpireTo::Despawn,
    };
    let (a, _obs, _disc) = rig_with(leave_room(), hold);
    let (tx, _reg) = channel::<RegistryMsg>(64);
    let mut a = a.with_registry(tx);
    let (_out, act) = join_with_actions(&mut a, ConnectionId(1));
    act.try_send(Action {
        conn: ConnectionId(1),
        player: PlayerId(0),
        op: crate::rpc::RPC_REQ_OP,
        payload: bytes::Bytes::from_static(&[1]),
    })
    .expect("the channel has room");
    step(&mut a, Instant::now(), 1, 30);
    assert!(a.conns[&PlayerId(1)].detached, "parked");
    assert!(act.is_closed(), "the membership ended");
    assert_eq!(
        a.m.requests_dropped_unread, 1,
        "the park's released channel held one unread request"
    );
}
