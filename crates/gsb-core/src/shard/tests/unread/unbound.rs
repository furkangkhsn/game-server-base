//! The room's B54 unbound count on the shard: what READ pulls under a
//! connection with no binding row is dropped at the translation and
//! counted by kind.

use super::*;

#[tokio::test]
async fn what_an_unbound_connection_sent_is_counted_by_kind_on_the_shard() {
    let mut a = bare_shard(0);
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: String::new(),
            out: out_tx,
            reply: reply_tx,
            claims: None,
        },
        1
    ));
    let (_entity, tx) = reply_rx.await.expect("delivered").expect("admitted");
    // One request and two actions under an unbound connection, then one
    // action under the bound one.
    two_requests_and_an_action(&tx, ConnectionId(99));
    tx.try_send(Action {
        conn: ConnectionId(1),
        player: PlayerId(0),
        op: 0x2001,
        payload: bytes::Bytes::new(),
    })
    .expect("room in the channel");
    assert!(a.step(&tinfo(2)));
    assert_eq!(a.m.requests_dropped_unbound, 2);
    assert_eq!(a.m.actions_dropped_unbound, 1);
    assert_eq!(a.m.actions_dropped_unread, 0, "pulled, not left unread");
    assert_eq!(
        a.sample().actions_dropped_unbound,
        1,
        "the sample carries it"
    );
}
