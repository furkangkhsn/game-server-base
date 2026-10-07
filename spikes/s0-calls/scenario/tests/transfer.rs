//! Friend gold transfer end to end (no-ECS flavour, so the test can read
//! the room's state between ticks): the reserve is written synchronously,
//! the bank charges once, the client is answered through `.answers`, and
//! a double click / a re-sent id cannot charge twice.

mod common;

use common::*;

fn alice(room: &mut Room<s0_calls::ecs::plain::PlainRoom<plain_game::Game>>) -> (u64, u64) {
    let p = room
        .logic()
        .game
        .players
        .values()
        .find(|p| p.player.conn == ALICE)
        .expect("alice joined");
    (p.wallet.gold, p.wallet.reserved)
}

#[test]
fn a_transfer_reserves_charges_once_and_is_answered_by_the_call() {
    let be = FakeBackend::new();
    let mut sim = sim(&be);
    let mut room = plain_game::room(RoomConfig::default(), Clients::from(&sim));
    let mut all = Answers::new();
    for t in 0..SCENARIO_TICKS {
        script(t, &room.inputs());
        for (c, r) in room.tick_sim(&mut sim) {
            all.push((t, c, r));
        }
        match t {
            2 => assert_eq!(alice(&mut room), (500, 0), "loaded"),
            // Tick 3: the request was taken, the reserve written in the
            // same system, the call issued. The bank already ran (test
            // mode runs services right after the tick) but its answer is
            // held for 3 ticks.
            3..=5 => {
                assert_eq!(alice(&mut room), (500, 100), "reserved while in flight");
                assert_eq!(be.charges(), 1);
            }
            6 => assert_eq!(alice(&mut room), (400, 0), "settled at tick 6's RESPONSES"),
            _ => {}
        }
    }

    // The first click: answered at tick 6 by the call itself (`.answers`).
    let first = answer(&all, ALICE, 10);
    assert_eq!(
        first.len(),
        2,
        "the answer, plus the engine's duplicate refusal"
    );
    let (t, _, ok) = first.iter().find(|(_, _, r)| r.ok).expect("answered");
    assert_eq!(*t, 6);
    assert_eq!(decode::<TransferDone>(ok).balance, 400);
    // The re-sent id 10 at tick 4: refused by the engine (in flight).
    let (t, _, dup) = first.iter().find(|(_, _, r)| !r.ok).expect("dup");
    assert_eq!(*t, 4);
    assert_eq!(dup.reason, s0_calls::wire::reason::DUPLICATE);
    // The double click (id 11, same tick): the game saw the transfer in
    // flight and said BUSY at once.
    let second = answer(&all, ALICE, 11);
    assert_eq!(second.len(), 1);
    assert!(second[0].2.game_error);
    assert_eq!(decode::<GameError>(&second[0].2).code, BUSY);

    assert_eq!(be.charges(), 1, "one charge");
    assert_eq!((be.gold("alice"), be.gold("bob")), (400, 200));
    let c = room.counters();
    assert_eq!(c.requests_duplicate, 1);
    assert_eq!(
        c.answers_sent, 5,
        "every .answers'd call reached its client"
    );
}

#[test]
fn an_unaffordable_transfer_never_calls() {
    let be = FakeBackend::new();
    let mut sim = sim(&be);
    let mut room = plain_game::room(RoomConfig::default(), Clients::from(&sim));
    let i = room.inputs();
    i.join(ALICE, "alice");
    for _ in 0..3 {
        room.tick_sim(&mut sim);
    }
    i.request(
        ALICE,
        rpc(
            1,
            &TransferGold {
                to: "bob".into(),
                amount: 501,
            },
        ),
    );
    let out = room.tick_sim(&mut sim);
    assert_eq!(out.len(), 1);
    assert_eq!(decode::<GameError>(&out[0].1).code, INSUFFICIENT);
    assert_eq!(room.counters().calls_issued, 1, "only the load");
}
