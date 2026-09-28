//! The room side's full-mailbox rule for close requests: a FULL registry
//! mailbox keeps them (in order) for the next tick, a CLOSED one drops
//! them — and counts each as a lost verdict (F56), as it does the leave
//! requests and the detach-despawn reports.

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

fn leave(conn: u64) -> LeaveRequest {
    LeaveRequest {
        conn: ConnectionId(conn),
        room: RoomId(1),
        entity: conn,
        park: None,
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
    let mut lost = VerdictsLost::default();
    flush_close_requests(&tx, &mut queue, &mut lost);
    assert_eq!(
        queue,
        vec![req(2), req(3)],
        "the refused two wait, in order"
    );
    assert_eq!(delivered(rx.try_recv().expect("one in")), req(1));

    flush_close_requests(&tx, &mut queue, &mut lost);
    assert_eq!(queue, vec![req(3)]);
    assert_eq!(delivered(rx.try_recv().expect("the next")), req(2));

    flush_close_requests(&tx, &mut queue, &mut lost);
    assert!(
        queue.is_empty(),
        "nothing is ever dropped while the registry lives"
    );
    assert!(lost.is_empty(), "nor counted lost");
    assert_eq!(delivered(rx.try_recv().expect("the last")), req(3));
}

/// The registry has stopped (its mailbox closed): nothing is left to
/// settle, so the queue does not keep retrying into the void — and each
/// dropped verdict is counted, the close by its reason (F56).
#[test]
fn a_closed_mailbox_drops_the_requests_and_counts_them() {
    let (tx, rx) = channel::<RegistryMsg>(4);
    drop(rx);
    let mut lost = VerdictsLost::default();
    let mut kick = req(2);
    kick.cause = ServerClose::Kicked;
    let mut queue = vec![req(1), kick];
    flush_close_requests(&tx, &mut queue, &mut lost);
    assert!(queue.is_empty());
    let mut leaves = vec![leave(3), leave(4), leave(5)];
    flush_leave_requests(&tx, &mut leaves, &mut lost);
    assert!(leaves.is_empty());
    let mut reports = vec![ConnectionId(6)];
    flush_despawn_reports(&tx, RoomId(1), &mut reports, &mut lost);
    assert!(reports.is_empty());
    assert_eq!(lost.closes.get(ServerClose::IdleInput), 1);
    assert_eq!(lost.closes.get(ServerClose::Kicked), 1);
    assert_eq!(lost.closes.total(), 2);
    assert_eq!((lost.leaves, lost.detach_despawns), (3, 1));
}

/// A FULL mailbox loses nothing: the leave requests and the reports wait
/// for the next tick like the close requests, and nothing is counted.
#[test]
fn a_full_mailbox_counts_nothing_lost() {
    let (tx, _rx) = channel::<RegistryMsg>(1);
    tx.try_send(RegistryMsg::Shutdown)
        .expect("fills the one slot");
    let mut lost = VerdictsLost::default();
    let mut leaves = vec![leave(3)];
    flush_leave_requests(&tx, &mut leaves, &mut lost);
    let mut reports = vec![ConnectionId(6)];
    flush_despawn_reports(&tx, RoomId(1), &mut reports, &mut lost);
    assert_eq!((leaves.len(), reports.len()), (1, 1), "both wait");
    assert!(lost.is_empty());
}
