//! The same transfer + inventory scenario written three ways — bevy
//! function systems, hecs, no ECS — gives identical client answers,
//! identical per-tick state hashes and identical service effects.

mod common;

use common::*;

struct Run {
    answers: Answers,
    hashes: Vec<u64>,
    counters: Counters,
    ledger: (i64, i64, u32, u32, u32, u64),
}

fn ledger(be: &FakeBackend) -> (i64, i64, u32, u32, u32, u64) {
    (
        be.gold("alice"),
        be.gold("bob"),
        be.count("alice", 1),
        be.count("alice", 3),
        be.count("bob", 1),
        be.charges(),
    )
}

macro_rules! run {
    ($flavour:ident) => {{
        let be = FakeBackend::new();
        let mut sim = sim(&be);
        let mut room = $flavour::room(RoomConfig::default(), Clients::from(&sim));
        room.hash_every_tick();
        let answers = run_sim(&mut room, &mut sim, SCENARIO_TICKS);
        Run {
            answers,
            hashes: room.hashes.clone(),
            counters: room.counters(),
            ledger: ledger(&be),
        }
    }};
}

#[test]
fn bevy_hecs_and_plain_agree_tick_for_tick() {
    let b = run!(bevy_game);
    let h = run!(hecs_game);
    let p = run!(plain_game);

    assert!(!b.answers.is_empty());
    assert_eq!(b.answers, h.answers, "bevy vs hecs: client answers");
    assert_eq!(b.answers, p.answers, "bevy vs plain: client answers");
    assert_eq!(b.hashes, h.hashes, "bevy vs hecs: per-tick state");
    assert_eq!(b.hashes, p.hashes, "bevy vs plain: per-tick state");
    assert_eq!(b.counters, h.counters);
    assert_eq!(b.counters, p.counters);
    assert_eq!(b.ledger, h.ledger);
    assert_eq!(b.ledger, p.ledger);
    // The scenario's service effects: one transfer charge (alice 500 →
    // 400, bob 100 → 200), one craft (two 1s → one 3), one grant to alice.
    assert_eq!(b.ledger.0, 400);
    assert_eq!(b.ledger.1, 200);
    assert_eq!(b.ledger.3, 1);
    assert_eq!(b.ledger.5, 1);
}
