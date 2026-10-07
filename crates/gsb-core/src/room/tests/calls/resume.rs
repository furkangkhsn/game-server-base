//! An explicit `calls.bind` survives a detach-hold and its resume
//! (BACKLOG B156): the parked row keeps the bind, the resumed session
//! inherits it, and the hold's end drops it with the row.

use super::*;

/// The owner the game binds alice's session to (not her player id).
const BOUND: Owner = Owner(4242);

fn held_rig() -> Rig {
    Rig::new(RigCfg {
        mode: Mode {
            hold: true,
            bind: Some((1, BOUND)),
            ..Mode::default()
        },
        ..Default::default()
    })
}

/// The owner every `Request` the game saw since the last call carried.
fn request_owners(rig: &mut Rig) -> Vec<Owner> {
    rig.seen()
        .into_iter()
        .filter_map(|s| match s {
            Seen::Request(_, _, owner) => Some(owner),
            _ => None,
        })
        .collect()
}

fn detach(rig: &mut Rig, conn: u64) {
    rig.room.handle_control(RoomControl::Detach {
        conn: ConnectionId(conn),
        entity: 1,
        identity: "alice".to_owned(),
    });
}

#[test]
fn an_explicit_bind_follows_the_session_through_a_resume() {
    let mut rig = held_rig();
    let alice = rig.join(1, "alice");
    rig.step(); // the game binds conn 1 → BOUND
    assert_eq!(rig.room.calls.bound(ConnectionId(1)), Some(BOUND));
    alice.request(1, 1001, &transfer("bob", 10));
    rig.step();
    assert_eq!(request_owners(&mut rig), [BOUND]);

    detach(&mut rig, 1);
    rig.step();
    assert_eq!(rig.room.m.resumes, 0);
    assert_eq!(
        rig.room.calls.bound(ConnectionId(1)),
        Some(BOUND),
        "the parked row keeps its bind"
    );

    let back = rig.join(2, "alice");
    assert_eq!(rig.room.m.resumes, 1, "a resume, not a fresh join");
    assert_eq!(rig.room.calls.bound(ConnectionId(1)), None);
    assert_eq!(rig.room.calls.bound(ConnectionId(2)), Some(BOUND));
    back.request(2, 1001, &transfer("bob", 10));
    rig.step();
    assert_eq!(
        request_owners(&mut rig),
        [BOUND],
        "the resumed session's request carries the bound owner, not the player"
    );
}

/// The hold's end takes the bind with the row: nothing lingers in the
/// table for a session that will never come back.
#[test]
fn a_parked_bind_ends_with_the_row() {
    let mut rig = held_rig();
    let _alice = rig.join(1, "alice");
    rig.step();
    detach(&mut rig, 1);
    rig.step();
    assert_eq!(rig.room.calls.bound(ConnectionId(1)), Some(BOUND));
    rig.leave(1);
    rig.step();
    assert_eq!(rig.room.calls.bound(ConnectionId(1)), None);
}
