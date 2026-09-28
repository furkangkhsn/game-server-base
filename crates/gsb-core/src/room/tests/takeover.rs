//! A resume that outruns the old session's detach (BACKLOG F32): the
//! identity is still bound to a LIVE row of another connection when the
//! resume lands. The resume takes that session over — the old session's
//! detach runs first, under the policy, and the resume consumes the park
//! it leaves — instead of seating a second entity for the identity while
//! the late detach parks the first one where nothing will resume it.

use super::binding::RebindLogic;
use super::*;

fn room() -> RoomActor<(), (), ()> {
    let (_tick_tx, tick_rx) = broadcast::channel(4);
    let (_control, control_rx) = channel(16);
    RoomActor::new(
        RoomConfig {
            id: RoomId(32),
            ..Default::default()
        },
        (),
        Box::new(RebindLogic::new()),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    )
}

/// An identified join (the registry routes every one through `Resume`):
/// the entity it was answered with.
fn resume(actor: &mut RoomActor<(), (), ()>, conn: u64, epoch: u64) -> EntityId {
    let (out, _o) = mpsc::channel::<FrameBatch>(2);
    let (reply, mut answer) = oneshot::channel();
    actor.handle_control(RoomControl::Resume {
        conn: ConnectionId(conn),
        epoch,
        identity: "ana".into(),
        out,
        reply,
    });
    answer.try_recv().expect("answered").expect("seated").0
}

fn detach(actor: &mut RoomActor<(), (), ()>, conn: u64, entity: EntityId) {
    actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(conn),
        entity,
        identity: "ana".into(),
    });
}

#[test]
fn a_resume_ahead_of_the_old_session_s_detach_takes_that_session_over() {
    let mut actor = room();
    let first = resume(&mut actor, 1, 1);
    let player = actor.binding[&ConnectionId(1)];

    // c2 reconnects before c1's detach reached the room.
    let second = resume(&mut actor, 2, 2);
    assert_eq!(second, first, "the SAME entity comes back");
    assert_eq!(actor.m.resumes, 1, "a resume, not a fresh join");
    assert_eq!(actor.conns.len(), 1, "one member for one identity");
    assert_eq!(actor.binding.get(&ConnectionId(2)), Some(&player));
    assert!(!actor.binding.contains_key(&ConnectionId(1)), "unbound");

    // c1's detach, late: its binding moved, nothing is parked.
    detach(&mut actor, 1, first);
    assert!(!actor.conns[&player].detached, "the new session stays live");
    assert_eq!(actor.conns.len(), 1);

    // The ledger holds no stale park: the next drop parks and resumes as
    // ever, onto the same entity.
    detach(&mut actor, 2, first);
    assert!(actor.conns[&player].detached, "parked");
    assert_eq!(resume(&mut actor, 3, 3), first);
    assert_eq!(actor.m.resumes, 2);
}

/// The same connection joining again is not a takeover: it supersedes
/// its own state with a fresh seat, as a rejoin always has.
#[test]
fn a_rejoin_of_the_same_connection_is_not_a_takeover() {
    let mut actor = room();
    let first = resume(&mut actor, 1, 1);
    let again = resume(&mut actor, 1, 2);
    assert_ne!(again, first, "a fresh seat");
    assert_eq!(actor.m.resumes, 0);
    assert_eq!(actor.conns.len(), 1);
}
