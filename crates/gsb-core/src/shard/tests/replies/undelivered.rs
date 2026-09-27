//! The room's B53 counts on the shard: a session that ends takes the
//! answers still owed to it (`requests_undelivered`) and its in-flight
//! requests (`requests_abandoned`) along, and both are counted — an
//! answer put back after failed sends only once.

use super::*;
use crate::rpc::RpcReply;

fn leave(a: &mut Shard, conn: u64, wire: u64) {
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn: ConnectionId(conn),
            entity: wire,
            epoch: 1,
        },
        2,
    ));
}

/// One answer owed (it rode dropped batches) and one request in flight
/// when the session leaves: one of each.
#[tokio::test]
async fn a_leave_counts_the_owed_answer_and_the_request_in_flight_on_the_shard() {
    let mut a = shard(4, 16);
    let (wire, tx, _slow) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    assert!(a.step(&tinfo(1)));
    request(&tx, 1, 2, OP_LOCAL);
    request(&tx, 1, 3, OP_EXT);
    assert!(a.step(&tinfo(2)));
    assert!(a.step(&tinfo(3)));
    assert_eq!(a.m.requests_undelivered, 0, "still owed, not lost");
    leave(&mut a, 1, wire);
    assert!(a.step(&tinfo(4)));
    assert_eq!(a.m.requests_undelivered, 1);
    assert_eq!(a.m.requests_abandoned, 1);
    let s = a.sample();
    assert_eq!((s.requests_undelivered, s.requests_abandoned), (1, 1));
}

/// Put back after every closed send, discarded once at the leave.
#[tokio::test]
async fn an_answer_put_back_after_closed_sends_is_counted_once_on_the_shard() {
    let mut a = shard(4, 16);
    let (wire, tx, gone) = join(&mut a, 1, 64);
    drop(gone);
    request(&tx, 1, 1, OP_LOCAL);
    for t in 1..=3 {
        assert!(a.step(&tinfo(t)));
    }
    assert_eq!(a.m.sends_closed, 3);
    leave(&mut a, 1, wire);
    assert!(a.step(&tinfo(4)));
    assert_eq!(a.m.requests_undelivered, 1, "once, not once per put-back");
}

/// An answer queued for a row gone the same tick: swept and counted at
/// the end of the fan-out.
#[tokio::test]
async fn an_answer_for_a_row_gone_this_tick_is_counted_on_the_shard() {
    let mut a = shard(4, 16);
    let (_wire, _tx, _rx) = join(&mut a, 1, 64);
    a.queued.insert(
        ConnectionId(9),
        vec![RpcReply {
            id: 7,
            ok: true,
            op: OP_LOCAL,
            reason: String::new(),
            payload: bytes::Bytes::new(),
        }],
    );
    assert!(a.step(&tinfo(1)));
    assert!(a.queued.is_empty());
    assert_eq!(a.m.requests_undelivered, 1);
}
