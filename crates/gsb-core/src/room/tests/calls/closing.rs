//! What nobody takes or answers is closed at tick end and counted
//! (SERVICES.md §5.4, spike `refusal` (3) + `binding`): untaken,
//! unanswered, unbound, untaken results, gone owners, late replies.

use super::*;

/// The game does not take its requests: the engine answers "not handled
/// this tick" at the end of the tick and counts it.
#[test]
fn an_untaken_request_is_closed_and_counted() {
    let mut rig = Rig::new(RigCfg {
        mode: Mode {
            take: false,
            ..Default::default()
        },
        ..Default::default()
    });
    let mut a = rig.join(1, "alice");
    a.request(7, 1001, &transfer("x", 1));
    rig.step();
    let got = a.answers();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].id, got[0].op), (7, 1001));
    assert_eq!(got[0].reason, crate::rpc::UNTAKEN_REASON);
    assert_eq!(rig.counts().requests_untaken, 1);
    // The id is free again: the next one is processed (and closed again).
    a.request(7, 1001, &transfer("x", 1));
    rig.step();
    assert_eq!(a.answers()[0].reason, crate::rpc::UNTAKEN_REASON);
    assert_eq!(rig.room.m.requests_rejected_dup, 0);
}

/// Taken, then the `ReplyTo` is dropped: closed at tick end as
/// unanswered.
#[test]
fn a_taken_unanswered_request_is_closed_and_counted() {
    let mut rig = Rig::new(RigCfg {
        mode: Mode {
            answer: false,
            ..Default::default()
        },
        ..Default::default()
    });
    let mut a = rig.join(1, "alice");
    a.request(3, 1001, &transfer("x", 1));
    rig.step();
    assert_eq!(a.answers()[0].reason, crate::rpc::UNANSWERED_REASON);
    let c = rig.counts();
    assert_eq!((c.requests_unanswered, c.requests_untaken), (1, 0));
}

/// A session with no owner: the engine answers, counts, and the game
/// never sees the request.
#[test]
fn an_unbound_session_is_answered_by_the_engine_and_never_seen() {
    let mut rig = Rig::new(RigCfg {
        mode: Mode {
            unbound: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let mut a = rig.join(1, "alice");
    a.request(3, 1001, &transfer("x", 1));
    rig.step();
    assert_eq!(a.answers()[0].reason, crate::rpc::NOT_BOUND_REASON);
    assert_eq!(rig.counts().requests_unbound, 1);
    assert!(!rig.seen().iter().any(|s| matches!(s, Seen::Request(..))));
}

/// An explicit `bind` overrides the default owner (and an undecodable
/// payload is answered by the engine).
#[test]
fn an_explicit_bind_overrides_the_default_owner() {
    let mut rig = Rig::new(RigCfg {
        mode: Mode {
            unbound: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let mut a = rig.join(1, "alice");
    rig.room.calls.bind(ConnectionId(1), Owner(1));
    a.request(3, 1001, &transfer("x", 1));
    a.raw(4, 1001, vec![0xFF, 0xFF]);
    rig.step();
    assert!(
        rig.seen()
            .contains(&Seen::Request(1001, "alice".into(), Owner(1)))
    );
    let got = a.answers();
    assert_eq!(got[0].reason, crate::rpc::UNDECODABLE_REASON);
    assert_eq!(rig.counts().requests_undecodable, 1);
    assert_eq!(rig.counts().requests_unbound, 0);
    // The session ends: its explicit binding goes with it.
    rig.leave(1);
    assert_eq!(rig.room.calls.bound(ConnectionId(1)), None);
}

/// Nobody takes the result: dropped at tick end and counted. The client
/// is still answered (`.answers`).
#[test]
fn an_untaken_result_is_dropped_and_counted() {
    let mut rig = Rig::new(RigCfg {
        mode: Mode {
            take_resolved: false,
            ..Default::default()
        },
        ..Default::default()
    });
    let mut a = rig.join(1, "alice");
    a.request(1, 1001, &transfer("x", 10));
    rig.step();
    rig.step();
    assert_eq!(balance(&a.answers()[0]), 490);
    let c = rig.counts();
    assert_eq!((c.responses_delivered, c.responses_unhandled), (1, 1));
}

/// The owner is gone when the result arrives: never handed out, counted;
/// the client is still answered.
#[test]
fn a_gone_owners_result_is_not_handed_out() {
    let mut rig = Rig::new(RigCfg {
        mode: Mode {
            owners_gone: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let mut a = rig.join(1, "alice");
    a.request(1, 1001, &transfer("x", 10));
    rig.step();
    rig.step();
    assert_eq!(balance(&a.answers()[0]), 490);
    let c = rig.counts();
    assert_eq!((c.responses_owner_gone, c.responses_delivered), (1, 0));
    assert!(!rig.seen().iter().any(|s| matches!(s, Seen::Resolved(..))));
}

/// The default owner test: a player who left is gone. Its call's result
/// is not handed out, and its answer — owed to a session that ended —
/// is abandoned, counted once.
#[test]
fn a_player_who_left_is_a_gone_owner_by_default() {
    let mut rig = Rig::new(RigCfg {
        bank_delay: 2,
        ..Default::default()
    });
    let a = rig.join(1, "alice");
    a.request(1, 1001, &transfer("x", 10));
    rig.step();
    rig.leave(1);
    assert_eq!(rig.room.m.requests_abandoned, 1, "the owed answer");
    rig.step();
    rig.step();
    let c = rig.counts();
    assert_eq!((c.results_ok, c.responses_owner_gone), (1, 1));
    assert_eq!(c.answers_sent, 0, "nobody to answer");
}

/// A reply to a request the engine already closed is ignored and
/// counted.
#[test]
fn a_late_reply_is_ignored_and_counted() {
    let mut calls = Calls::default();
    calls.accept::<Ping>().expect("boot");
    let mut m = crate::room::RoomCounters::default();
    let from = crate::calls::Identity::new("p");
    let ping = Ping { n: 1 }.encode_to_vec();
    calls.read(
        ConnectionId(1),
        PlayerId(1),
        &from,
        Some(Owner(1)),
        9,
        1007,
        &ping,
    );
    let r = calls.take_incoming::<Ping>().pop().expect("one");
    calls.end_tick(); // closed: unanswered
    calls.reply(r.reply, Ok(Ping { n: 1 }));
    let c = *calls.counts();
    assert_eq!((c.requests_unanswered, c.requests_replied_late), (1, 1));
    assert_eq!(
        calls.take_outbox().len(),
        1,
        "one engine answer, no second one"
    );
    calls.drop_all_at_stop(&mut m);
}

/// An explicit bind wins over the engine's default owner (the player).
#[test]
fn an_explicit_bind_wins_over_the_default_owner() {
    let mut rig = Rig::new(RigCfg {
        mode: Mode {
            answer: false,
            ..Default::default()
        },
        ..Default::default()
    });
    let a = rig.join(1, "alice");
    rig.room.calls.bind(ConnectionId(1), Owner(42));
    a.request(3, 1001, &transfer("x", 1));
    rig.step();
    assert!(
        rig.seen()
            .contains(&Seen::Request(1001, "alice".into(), Owner(42)))
    );
}
