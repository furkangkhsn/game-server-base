//! (e) A fake service on the test clock: "the service answers 3 ticks
//! later" is a test of a few lines, with no runtime and no HTTP — and so
//! are the timeout and the abandoned reply.

mod common;

use common::*;
use s0_calls::calls::{CallError, Done, Outcome};
use s0_calls::fake::FakeService;
use s0_calls::room::{Logic, Room, RoomConfig, TickCtx};
use s0_calls::service::{Reply, ServiceRef};

/// Calls once on tick `at`, records (tick, outcome) for every `Done`.
struct Probe {
    econ: ServiceRef<EconomyMsg>,
    at: u64,
    seen: Vec<(u64, Outcome<GrantGold>)>,
}

impl Logic for Probe {
    type Done = Outcome<GrantGold>;
    type Req = ();
    fn on_done(&mut self, ctx: &TickCtx<'_, Self::Done>, done: &mut Vec<Done<Self::Done>>) {
        for d in done.drain(..) {
            self.seen.push((ctx.tick, d.msg));
        }
    }
    fn on_request(&mut self, _ctx: &TickCtx<'_, Self::Done>, _req: ()) {}
    fn update(&mut self, ctx: &TickCtx<'_, Self::Done>) {
        if ctx.tick == self.at {
            ctx.call(&self.econ, GrantGold { amount: 5 }, |o| o)
                .expect("room for one call");
        }
    }
}

fn probe(timeout_ticks: u64) -> (Room<Probe>, FakeService<EconomyMsg>) {
    let (econ, fake) = FakeService::new("economy", 8);
    let mut cfg = RoomConfig::default();
    cfg.calls.timeout_ticks = timeout_ticks;
    let room = Room::new(
        cfg,
        Probe {
            econ,
            at: 2,
            seen: Vec::new(),
        },
        Default::default(),
    );
    (room, fake)
}

#[test]
fn an_answer_three_ticks_later_is_seen_on_the_next_responses_phase() {
    let (mut room, mut fake) = probe(100);
    let mut ledger = Ledger::with_balance(0);
    for _ in 0..8 {
        room.tick();
        fake.step(room.now(), |m, s| ledger.handle(3, m, s));
    }
    // Called in tick 2's SYSTEMS; the service took it after tick 2,
    // answered after tick 5; the room saw it in tick 6's RESPONSES.
    let seen = &room.logic().seen;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, 6);
    assert_eq!(seen[0].1.as_ref().unwrap().balance, 5);
}

#[test]
fn no_answer_times_out_and_a_later_answer_is_counted_late() {
    let (mut room, mut fake) = probe(4);
    let mut held: Vec<Reply<GrantGold>> = Vec::new();
    for _ in 0..8 {
        room.tick();
        fake.step(room.now(), |m, _| match m {
            EconomyMsg::Grant(c) => held.push(c.reply),
            EconomyMsg::Buy(_) => unreachable!(),
        });
    }
    let seen = &room.logic().seen;
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0],
        (6, Err(CallError::TimedOut)),
        "called at 2, due at 2 + 4"
    );
    // The service finally answers: absorbed, counted, never a second Done.
    let late = held.pop().unwrap();
    late.send(Ok(BuyResult { balance: 1 })).unwrap();
    room.tick();
    assert_eq!(room.logic().seen.len(), 1);
    let c = room.calls().counters();
    assert_eq!(c.calls_timed_out, 1);
    assert_eq!(c.calls_late, 1);
}

#[test]
fn a_dropped_reply_is_abandoned_at_once() {
    let (mut room, mut fake) = probe(100);
    for _ in 0..4 {
        room.tick();
        fake.step(room.now(), |m, _| drop(m)); // the service "dies" mid-call
    }
    let seen = &room.logic().seen;
    assert_eq!(seen.as_slice(), &[(3, Err(CallError::Abandoned))]);
    assert_eq!(room.calls().counters().calls_abandoned, 1);
}
