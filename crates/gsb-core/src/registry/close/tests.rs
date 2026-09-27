//! The room side's full-mailbox rule for close requests: a FULL registry
//! mailbox keeps them (in order) for the next tick, a CLOSED one drops
//! them.

use super::*;
use crate::channel::channel;

fn req(conn: u64) -> CloseRequest {
    CloseRequest {
        conn: ConnectionId(conn),
        room: RoomId(1),
        entity: conn,
        parked: false,
        cause: ServerClose::IdleInput,
        reason: format!("input idle #{conn}"),
    }
}

fn delivered(msg: RegistryMsg) -> CloseRequest {
    match msg {
        RegistryMsg::CloseConn(r) => r,
        other => panic!("not a close request: {other:?}"),
    }
}

/// A mailbox with room for one takes the first request; the other two
/// wait, in order, and go out on the flushes after the registry drained.
#[test]
fn a_full_mailbox_keeps_the_requests_for_the_next_tick() {
    let (tx, mut rx) = channel::<RegistryMsg>(1);
    let mut queue = vec![req(1), req(2), req(3)];
    flush_close_requests(&tx, &mut queue);
    assert_eq!(
        queue,
        vec![req(2), req(3)],
        "the refused two wait, in order"
    );
    assert_eq!(delivered(rx.try_recv().expect("one in")), req(1));

    flush_close_requests(&tx, &mut queue);
    assert_eq!(queue, vec![req(3)]);
    assert_eq!(delivered(rx.try_recv().expect("the next")), req(2));

    flush_close_requests(&tx, &mut queue);
    assert!(
        queue.is_empty(),
        "nothing is ever dropped while the registry lives"
    );
    assert_eq!(delivered(rx.try_recv().expect("the last")), req(3));
}

/// The registry is gone (the process is coming down): nothing is left to
/// settle, so the queue does not keep retrying into the void.
#[test]
fn a_closed_mailbox_drops_the_requests() {
    let (tx, rx) = channel::<RegistryMsg>(4);
    drop(rx);
    let mut queue = vec![req(1), req(2)];
    flush_close_requests(&tx, &mut queue);
    assert!(queue.is_empty());
}
