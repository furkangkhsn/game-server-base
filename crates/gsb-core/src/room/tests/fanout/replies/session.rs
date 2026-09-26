//! Undelivered answers are session-scoped (F14), like every RPC
//! state: a leave or a detach takes them along — nothing stays queued,
//! and neither the old channel nor a resumed session receives them.

use super::*;

/// A connection that leaves with undelivered answers takes them along:
/// nothing stays queued, and nothing more reaches its old channel.
#[test]
fn a_leaving_connection_takes_its_undelivered_answers_along() {
    let (mut a, _control) = room(4, 16);
    let (tx, mut rx) = join(&mut a, 1, 1);
    let (_tx2, mut rx2) = join(&mut a, 2, 64);
    request(&tx, 1, 1, OP_LOCAL);
    step(&mut a, 1);
    request(&tx, 1, 2, OP_LOCAL);
    step(&mut a, 2);
    assert!(a.queued.contains_key(&ConnectionId(1)), "2 is owed");
    a.handle_control(RoomControl::Leave {
        conn: ConnectionId(1),
        entity: 1,
    });
    step(&mut a, 3);
    assert!(a.queued.is_empty(), "nothing leaks");
    assert_eq!(drain(&mut rx), [vec![1]]);
    assert_eq!(drain(&mut rx2).len(), 3, "the other member is unaffected");
}

/// Undelivered answers are session-scoped like every RPC state: a
/// detach drops them and the resumed session's fresh transport never
/// receives the dead session's answers.
#[test]
fn a_resumed_session_does_not_inherit_undelivered_answers() {
    let (mut a, _control) = room(4, 16);
    let (tx, _slow) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    step(&mut a, 1);
    request(&tx, 1, 2, OP_LOCAL);
    step(&mut a, 2);
    a.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity: 1,
        identity: "one".into(),
    });
    let (out, mut fresh) = mpsc::channel::<FrameBatch>(64);
    let (reply, mut replied) = oneshot::channel();
    a.handle_control(RoomControl::Resume {
        conn: ConnectionId(2),
        epoch: 0,
        identity: "one".into(),
        out,
        reply,
    });
    replied.try_recv().expect("sync reply").expect("resumed");
    step(&mut a, 3);
    assert_eq!(drain(&mut fresh), [Vec::<u64>::new()]);
    assert!(a.queued.is_empty());
}
