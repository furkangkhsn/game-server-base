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

/// A recorded room replays the park and the bind's move (feature
/// `record`): the replayed game sees the resumed session's request under
/// the bound owner, exactly as the live one did.
#[cfg(feature = "record")]
#[test]
fn a_recorded_resume_replays_the_bind_it_carried() {
    use crate::determinism::{InputLog, Recorder, Replay, Sim, SimRoom};
    use std::sync::Arc;

    let game = |seen| Game {
        bank: Bank::new(4, 1).0,
        friends: None,
        mode: Mode {
            hold: true,
            bind: Some((1, BOUND)),
            ..Mode::default()
        },
        wallets: BTreeMap::new(),
        seen,
        boot_twice: false,
        parked: BTreeMap::new(),
    };
    let owners = |rx: &smpsc::Receiver<Seen>| -> Vec<Owner> {
        rx.try_iter()
            .filter_map(|s| match s {
                Seen::Request(_, _, o) => Some(o),
                _ => None,
            })
            .collect()
    };
    let (tx, live_seen) = smpsc::channel();
    let mut room = SimRoom::new(RoomConfig::default(), (), Box::new(game(tx)));
    let (recorder, mut inbox) = Recorder::channel(1 << 10, "resume");
    room.record(recorder);
    let mut sim = Sim::new();
    let alice = room.join(1, "alice");
    room.tick(&mut sim);
    alice.request(1, &transfer("bob", 10));
    room.tick(&mut sim);
    room.disconnect(&alice);
    room.tick(&mut sim);
    let back = room.join(2, "alice");
    back.request(2, &transfer("bob", 10));
    room.tick(&mut sim);
    let live = owners(&live_seen);
    assert_eq!(live, [BOUND, BOUND]);

    let log = InputLog::collect(&mut inbox).expect("the header arrived");
    // Through a file too: the two inputs encode and decode.
    let path = std::env::temp_dir().join(format!("gsb-q5-{}-resume.log", std::process::id()));
    crate::determinism::write_log(&path, &log, room.calls().codecs()).expect("written");
    let read = crate::determinism::read_log(&path).expect("read");
    let _ = std::fs::remove_file(&path);
    for log in [Arc::new(log), Arc::new(read)] {
        let (tx, replay_seen) = smpsc::channel();
        let mut replay = Replay::new(log, (), Box::new(game(tx)));
        while replay.tick().is_some() {}
        let report = replay.run();
        assert_eq!(report.counts.replay_diverged, 0);
        assert_eq!(owners(&replay_seen), live, "the bind replays");
    }
}
