//! What a stopping shard still holds (BACKLOG B62), the room's rule per
//! shard: the unread input of its rows, the answers it owes, the
//! requests in flight — counted and handed to the collector in the
//! shard's final sample, under the shard's own sample id.

use std::collections::VecDeque;
use std::time::Instant;

use super::*;
use crate::rpc::{PendingRequest, RpcReply};

#[tokio::test]
async fn a_stopping_shard_counts_what_it_holds_in_its_final_sample() {
    let mut a = bare_shard(1);
    let (metrics, mut samples) = mpsc::channel(8);
    a.metrics = metrics;
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: String::new(),
            out: out_tx,
            reply: reply_tx
        },
        1
    ));
    let (_entity, actions) = reply_rx.await.expect("delivered").expect("admitted");
    two_requests_and_an_action(&actions, ConnectionId(1));
    a.queued.insert(
        ConnectionId(1),
        vec![RpcReply {
            id: 3,
            ok: true,
            op: 1,
            reason: String::new(),
            payload: bytes::Bytes::new(),
        }],
    );
    a.pending.insert(
        ConnectionId(1),
        VecDeque::from([PendingRequest {
            id: 4,
            op: 1,
            due: Instant::now(),
        }]),
    );
    a.pending_total = 1;

    a.finish();

    let mut last = None;
    while let Ok(ev) = samples.try_recv() {
        last = Some(ev);
    }
    let Some(MetricsEvent::RoomFinal(s)) = last else {
        panic!("the final sample is the shard's last event: {last:?}");
    };
    assert_eq!(s.room, RoomId((9 << 16) + 1), "the shard's own sample id");
    assert_eq!(s.requests_dropped_unread, 2);
    assert_eq!(s.actions_dropped_unread, 1);
    assert_eq!(s.requests_undelivered, 1);
    assert_eq!(s.requests_abandoned, 1);
    assert_eq!(s.pending_requests, 0);
}
