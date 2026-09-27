//! The requests still unread in a session's channel when the ceiling's
//! default action ends the membership (B36's ledger on the B40 path):
//! the idle sweep runs before READ, so a request sent right before it is
//! still in the channel. A despawn drops the row and counts it; a PARK
//! left behind releases the channel it shared with the live connection
//! (`release_actions`) — and must count it the same way.

use super::*;

/// An RPC request into the member's action channel, left unread.
fn request(act: &Mailbox<Action>) {
    act.try_send(Action {
        conn: ConnectionId(1),
        player: PlayerId(0),
        op: crate::rpc::RPC_REQ_OP,
        payload: bytes::Bytes::from_static(&[1]),
    })
    .expect("the channel has room");
}

/// A plain game action beside it (B54: counted apart).
fn action(act: &Mailbox<Action>) {
    act.try_send(Action {
        conn: ConnectionId(1),
        player: PlayerId(0),
        op: 0x2001,
        payload: bytes::Bytes::new(),
    })
    .expect("the channel has room");
}

#[test]
fn a_request_unread_when_a_despawn_is_left_behind_is_counted() {
    let mut r = Rig::new(leave_room(75), Detach::Despawn);
    let _reg = registry(&mut r, 64);
    let (_entity, act) = r.join(ConnectionId(1), "ana");
    request(&act);
    r.step_at(1, 30);
    assert!(act.is_closed(), "the membership ended");
    assert_eq!(r.actor.m.requests_dropped_unread, 1);
}

#[test]
fn a_request_unread_when_a_park_is_left_behind_is_counted() {
    let mut r = Rig::new(leave_room(76), PARK);
    let _reg = registry(&mut r, 64);
    let (_entity, act) = r.join(ConnectionId(1), "ana");
    request(&act);
    action(&act);
    r.step_at(1, 30);
    assert!(r.actor.conns[&PlayerId(1)].detached, "parked");
    assert!(act.is_closed(), "the membership ended");
    assert_eq!(
        r.actor.m.requests_dropped_unread, 1,
        "the park's released channel held one unread request"
    );
    assert_eq!(
        r.actor.m.actions_dropped_unread, 1,
        "and one plain action, counted apart (B54)"
    );
}
