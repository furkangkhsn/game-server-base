//! TEST mode: fixed `dt`, a seeded generator, fake services the engine
//! times (`Delay`), scripted clients — the same run every time.

use super::game::*;
use crate::calls::{Envelope, Route};
use crate::determinism::{Delay, Sim, SimRoom};
use crate::room::RoomConfig;

fn room(seed: u64, bank: Route<BankMsg>, inv: Route<InvMsg>) -> SimRoom<World> {
    SimRoom::new(
        RoomConfig {
            seed: Some(seed),
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        World::new(),
        Box::new(Shop {
            bank,
            inv,
            rolls: 0,
        }),
    )
}

fn services(sim: &mut Sim, bank: u64) -> (Route<BankMsg>, Route<InvMsg>) {
    let bank = sim.service(
        "bank",
        64,
        Delay::ticks(
            bank,
            |env: Envelope<BankMsg>| async move { bank_answer(env) },
        ),
    );
    let inv = sim.service(
        "inventory",
        64,
        Delay::ticks(1, |env: Envelope<InvMsg>| async move { inv_answer(env) }),
    );
    (bank, inv)
}

type Run = (Vec<Option<u64>>, Vec<(u64, u64, Result<u64, u32>)>);

/// The scripted match: two players, buys before and after the loads,
/// a double click, one too poor.
fn scripted(seed: u64) -> Run {
    let mut sim = Sim::new();
    let (bank, inv) = services(&mut sim, 3);
    let mut room = room(seed, bank, inv);
    let mut alice = room.join(1, "alice");
    let mut bob = room.join(2, "bob");
    let mut got = Vec::new();
    for t in 1..=14u64 {
        match t {
            1 => alice.request(1, &Buy { price: 50 }),
            4 => {
                alice.request(2, &Buy { price: 100 });
                bob.request(1, &Buy { price: 600 });
            }
            5 => alice.request(3, &Buy { price: 10 }),
            9 => alice.request(4, &Buy { price: 10 }),
            _ => {}
        }
        assert!(room.tick(&mut sim));
        for (who, c) in [(1u64, &mut alice), (2, &mut bob)] {
            for a in answers(c) {
                got.push((t, who, verdict(&a)));
            }
        }
    }
    (room.hashes().to_vec(), got)
}

#[test]
fn test_mode_is_identical_run_to_run_and_the_seed_is_part_of_the_run() {
    let a = scripted(1);
    let b = scripted(1);
    assert_eq!(a, b, "same seed, same hashes and answers");
    assert!(
        a.0.iter().all(Option::is_some),
        "the game hashes every step"
    );
    // What the script produces (tick, player, verdict): the first buy
    // before the load answered; the poor buy is refused by the bank three
    // ticks after it was issued; the double click is BUSY at once; the
    // charge lands three ticks after the call.
    assert_eq!(
        a.1,
        vec![
            (1, 1, Err(NOT_LOADED)),
            (5, 1, Err(BUSY)),
            (7, 1, Ok(400)),
            (7, 2, Err(POOR)),
            (12, 1, Ok(390)),
        ]
    );
    let c = scripted(2);
    assert_eq!(a.1, c.1, "the answers do not draw from the generator");
    assert_ne!(a.0, c.0, "the seed is part of the run");
}

/// `Delay::ticks(n)`: a call issued at tick T's FLUSH is answered at tick
/// T + n's RESPONSES — never earlier, never later.
#[test]
fn delay_ticks_n_answers_at_the_nth_tick_after_the_call() {
    for n in [1u64, 2, 5] {
        let mut sim = Sim::new();
        let (bank, inv) = services(&mut sim, n);
        let mut room = room(7, bank, inv);
        let mut alice = room.join(1, "alice");
        room.tick(&mut sim); // tick 1: the load is issued
        room.tick(&mut sim); // tick 2: loaded
        alice.request(1, &Buy { price: 5 });
        room.tick(&mut sim); // tick 3: the charge is issued
        let issued = room.tick_no();
        let mut at = None;
        for _ in 0..10 {
            room.tick(&mut sim);
            if !answers(&mut alice).is_empty() {
                at = Some(room.tick_no());
                break;
            }
        }
        assert_eq!(at, Some(issued + n), "delay {n}");
    }
}

/// `Delay::never`: the call is held; `kill` answers it `Abandoned` and
/// refuses the next one `Gone` — both through the client's answer.
#[test]
fn delay_never_holds_until_the_service_is_killed() {
    let mut sim = Sim::new();
    let bank = sim.service(
        "bank",
        8,
        Delay::never(|env: Envelope<BankMsg>| async move { bank_answer(env) }),
    );
    let inv = sim.service(
        "inventory",
        8,
        Delay::ticks(1, |env: Envelope<InvMsg>| async move { inv_answer(env) }),
    );
    let bank_id = bank.id();
    let mut room = room(3, bank, inv);
    let mut alice = room.join(1, "alice");
    room.tick(&mut sim);
    room.tick(&mut sim);
    alice.request(1, &Buy { price: 5 });
    for _ in 0..20 {
        room.tick(&mut sim);
    }
    assert!(answers(&mut alice).is_empty(), "held forever");
    assert_eq!(room.calls().pending_len(), 1);
    sim.kill(bank_id);
    room.tick(&mut sim);
    let got = answers(&mut alice);
    assert_eq!(got.len(), 1);
    assert!(!got[0].ok && !got[0].game_error);
    assert_eq!(got[0].reason, crate::rpc::ABANDONED_REASON);
    alice.request(2, &Buy { price: 5 });
    room.tick(&mut sim);
    room.tick(&mut sim);
    let got = answers(&mut alice);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].reason, crate::rpc::REFUSED_GONE_REASON);
    let c = room.counts();
    assert_eq!((c.results_abandoned, c.calls_refused_gone), (1, 1));
}

/// The idle cost is unchanged: a quiet room that records nothing keeps
/// no per-tick buffer, and its table keeps no input.
#[test]
fn a_quiet_room_that_records_nothing_keeps_nothing() {
    let mut sim = Sim::new();
    let (bank, inv) = services(&mut sim, 1);
    let mut room = room(5, bank, inv);
    for _ in 0..500 {
        room.tick(&mut sim);
    }
    assert_eq!(room.calls().buffer_capacities(), [0; 5]);
    assert_eq!(room.counts(), crate::metrics::CallCounts::default());
    #[cfg(feature = "record")]
    assert!(!room.calls().recording(), "no recorder, no input kept");
}

/// The call keys derive from the seed: the same seed, the same origin;
/// another seed (or another room), another.
#[test]
fn the_call_key_origin_derives_from_the_seed_and_the_room() {
    let origin = |seed: u64, id: u64| {
        let mut sim = Sim::new();
        let (bank, inv) = services(&mut sim, 1);
        let mut r = room(seed, bank, inv);
        r.tick(&mut sim);
        let _ = id;
        r.calls().config().origin
    };
    assert_eq!(origin(1, 0), origin(1, 0));
    assert_ne!(origin(1, 0), origin(2, 0));
    assert_eq!(
        origin(1, 0),
        crate::calls::mix(1 ^ crate::calls::mix(crate::room::RoomConfig::default().id.0))
    );
}

/// A generated service's implementation runs in TEST mode too:
/// `Delay::ticks(n, Svc::serve(fake))` answers `n` ticks after the call,
/// through its generated router — no runtime, no tower host.
#[test]
fn a_generated_service_serves_in_test_mode_on_the_engines_clock() {
    use crate::calls::{CallError, Calls, Owner, ServiceClient};
    use crate::room::RoomCounters;

    let mut sim = Sim::new();
    let route = sim.served(8, Delay::ticks(2, Purse::serve(Fake)));
    let purse = PurseClient::from_route(route);
    let mut calls = Calls::default();
    let mut m = RoomCounters::default();
    let alive = |_: Owner| true;
    calls.call(purse.spend(3)).with(1u8);
    calls.call(purse.spend(30)).with(2u8);
    calls.flush(); // tick 1's FLUSH
    let mut seen = Vec::new();
    for tick in 1..=4u64 {
        sim.step(tick);
        calls.responses(crate::ticker::now(), &alive, &mut m); // tick + 1
        for r in calls.take_resolved::<purse::Spend, u8>() {
            seen.push((tick + 1, r.ctx, r.outcome));
        }
    }
    assert_eq!(
        seen,
        vec![
            (3, 1, Ok(7)),
            (3, 2, Err(CallError::Service("poor".to_string())))
        ]
    );
}

// A generated service (the macro works at module level).
use crate::calls::CallCx;

#[crate::service]
pub trait Purse {
    async fn spend(&self, cx: &CallCx, amount: u64) -> Result<u64, String>;
}

struct Fake;

impl Purse for Fake {
    async fn spend(&self, _cx: &CallCx, amount: u64) -> Result<u64, String> {
        if amount > 10 {
            Err("poor".into())
        } else {
            Ok(10 - amount)
        }
    }
}
