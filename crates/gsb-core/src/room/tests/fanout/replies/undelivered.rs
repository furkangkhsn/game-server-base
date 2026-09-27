//! What a session that ended takes with it, counted (BACKLOG B53): the
//! answers the room still owed it (`requests_undelivered`) and its
//! requests still in flight (`requests_abandoned`). The request itself
//! is already in its own bucket; these count what became of its answer.

use super::*;
use crate::rpc::RpcReply;

fn leave(a: &mut RoomActor<(), (), ()>, conn: u64) {
    a.handle_control(RoomControl::Leave {
        conn: ConnectionId(conn),
        entity: conn,
    });
}

/// Answer 2 rides two dropped batches (put back each time, F14 — still
/// owed, not lost); the session then leaves: discarded, counted once.
#[test]
fn an_answer_owed_at_a_leave_is_counted_undelivered() {
    let (mut a, _control) = room(4, 16);
    let (tx, _slow) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    step(&mut a, 1);
    request(&tx, 1, 2, OP_LOCAL);
    step(&mut a, 2);
    step(&mut a, 3);
    assert_eq!(a.m.requests_undelivered, 0, "still owed, not lost");
    leave(&mut a, 1);
    step(&mut a, 4);
    assert_eq!(a.m.requests_undelivered, 1);
    assert_eq!(a.m.requests_abandoned, 0);
    assert_eq!(a.sample().requests_undelivered, 1, "the sample carries it");
}

/// The connection is gone before the room learns it: every tick the
/// answer meets the closed channel and goes back. However often it went
/// back, it is ONE undelivered answer when the leave discards it.
#[test]
fn an_answer_put_back_after_closed_sends_is_counted_once() {
    let (mut a, _control) = room(4, 16);
    let (tx, gone) = join(&mut a, 1, 64);
    drop(gone);
    request(&tx, 1, 1, OP_LOCAL);
    for t in 1..=3 {
        step(&mut a, t);
    }
    assert_eq!(a.m.sends_closed, 3);
    assert_eq!(a.m.requests_undelivered, 0);
    leave(&mut a, 1);
    step(&mut a, 4);
    assert_eq!(a.m.requests_undelivered, 1, "once, not once per put-back");
}

/// A request still in flight when the session leaves: abandoned, not an
/// undelivered answer (there is none yet). (A runtime: the worker is a
/// task.)
#[tokio::test]
async fn a_request_in_flight_at_a_leave_is_counted_abandoned() {
    let (mut a, _control) = room(4, 16);
    let (tx, _rx) = join(&mut a, 1, 64);
    request(&tx, 1, 1, OP_EXT);
    step(&mut a, 1);
    assert_eq!(a.pending_total, 1);
    leave(&mut a, 1);
    step(&mut a, 2);
    assert_eq!(a.m.requests_abandoned, 1);
    assert_eq!(a.m.requests_undelivered, 0);
    assert_eq!(a.sample().requests_abandoned, 1, "the sample carries it");
}

/// A detach takes both along (RPC state is session-scoped): the owed
/// answer and the in-flight request are each counted once.
#[tokio::test]
async fn a_detach_counts_both() {
    let (mut a, _control) = room(4, 16);
    let (tx, _slow) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    step(&mut a, 1);
    request(&tx, 1, 2, OP_LOCAL);
    request(&tx, 1, 3, OP_EXT);
    step(&mut a, 2);
    a.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity: 1,
        identity: "one".into(),
    });
    step(&mut a, 3);
    assert_eq!(a.m.requests_undelivered, 1);
    assert_eq!(a.m.requests_abandoned, 1);
}

/// An answer queued for a row that left the table the same tick is
/// swept at the end of the fan-out — and counted there.
#[test]
fn an_answer_for_a_row_gone_this_tick_is_counted_undelivered() {
    let (mut a, _control) = room(4, 16);
    let (_tx, _rx) = join(&mut a, 1, 64);
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
    step(&mut a, 1);
    assert!(a.queued.is_empty());
    assert_eq!(a.m.requests_undelivered, 1);
}
