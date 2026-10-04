//! Every ending comes back through `take_resolved` — including the
//! engine's refusals (full mailbox, service gone, share used up), which
//! are decided at the tick's flush and delivered in the NEXT tick's
//! RESPONSES like any answer. What nobody takes or answers is closed at
//! tick end and counted by name.

mod common;

use common::*;
use s0_calls::wire::reason;

#[derive(Clone, PartialEq, prost::Message)]
pub struct Ping {
    #[prost(uint32, tag = "1")]
    pub n: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Pong {
    #[prost(uint32, tag = "1")]
    pub n: u32,
}
impl Call for Ping {
    type Response = Pong;
    type Error = GameError;
}
impl Request for Ping {
    const OP: u16 = 2001;
}

#[service]
pub trait Echo {
    async fn echo(&self, cx: &CallCx, n: u32) -> Result<Pong, GameError>;
}

struct E;
impl Echo for E {
    async fn echo(&self, _cx: &CallCx, n: u32) -> Result<Pong, GameError> {
        Ok(Pong { n })
    }
}

#[derive(Default)]
struct S {
    echo: Option<EchoClient>,
    /// Calls to issue this tick.
    fire: u32,
    /// Answer client pings with the call (`.answers`).
    answer_pings: bool,
    /// Take results?
    take: bool,
    seen: Vec<(Owner, Result<u32, CallError<GameError>>)>,
}

fn sys(s: &mut S, calls: &mut Calls, _ctx: &mut TickCtx) {
    let echo = s.echo.clone().expect("client");
    if s.take {
        for r in calls.take_resolved::<echo::Echo, ()>() {
            s.seen.push((r.owner, r.outcome.map(|p| p.n)));
        }
    }
    for n in 0..std::mem::take(&mut s.fire) {
        calls.call(echo.echo(n)).owner(Owner(u64::from(n)));
    }
    if s.answer_pings {
        for r in calls.take_incoming::<Ping>() {
            calls.call(echo.echo(r.body.n)).answers(r.reply);
        }
    }
}

fn room(sim: &Sim, share: u32) -> ScriptedRoom<S> {
    let s = S {
        echo: Some(sim.client()),
        take: true,
        ..S::default()
    };
    scripted(
        RoomConfig {
            share,
            ..RoomConfig::default()
        },
        s,
        |c| c.accept::<Ping>().unwrap(),
        sys,
    )
}

#[test]
fn a_full_mailbox_refuses_through_take_resolved_next_tick() {
    let mut sim = Sim::new();
    sim.service(Echo::serve(E), 2); // mailbox of 2
    let mut room = room(&sim, 64);
    room.logic_mut().game.s.fire = 3;
    room.tick_sim(&mut sim);
    assert!(
        room.logic().game.s.seen.is_empty(),
        "never in the issuing tick"
    );
    room.tick_sim(&mut sim);
    let seen = &room.logic().game.s.seen;
    // Refusals were decided at tick 0's flush → first in tick 1's
    // RESPONSES; then the two answers.
    assert_eq!(
        seen,
        &vec![
            (Owner(2), Err(CallError::Refused(Refusal::Full))),
            (Owner(0), Ok(0)),
            (Owner(1), Ok(1)),
        ]
    );
    let c = room.counters();
    assert_eq!((c.calls_sent, c.calls_refused_full), (2, 1));
    assert_eq!((c.results_ok, c.results_refused), (2, 1));
}

#[test]
fn the_share_caps_one_actor_and_refuses_the_rest() {
    let mut sim = Sim::new();
    sim.service(Delay::ticks(3, Echo::serve(E)), 64);
    let mut room = room(&sim, 1);
    room.logic_mut().game.s.fire = 2;
    for _ in 0..4 {
        room.tick_sim(&mut sim);
    }
    let seen = &room.logic().game.s.seen;
    assert_eq!(seen[0], (Owner(1), Err(CallError::Refused(Refusal::Share))));
    assert_eq!(seen[1], (Owner(0), Ok(0)));
    assert_eq!(room.counters().calls_refused_share, 1);
}

#[test]
fn a_gone_service_refuses_new_calls_and_abandons_held_ones() {
    let mut sim = Sim::new();
    sim.service(Delay::never(Echo::serve(E)), 64);
    let mut room = room(&sim, 64);
    let i = room.inputs();
    i.join(ALICE, "alice");
    room.logic_mut().game.s.fire = 1;
    room.logic_mut().game.s.answer_pings = true;
    i.request(ALICE, rpc(7, &Ping { n: 9 }));
    room.tick_sim(&mut sim); // two calls held by a never-answering service
    sim.kill::<echo::Kind>(); // it dies: held calls → Abandoned
    room.logic_mut().game.s.fire = 1; // and a new call → Gone
    let out = room.tick_sim(&mut sim);
    // The client's call was abandoned: the engine answers it.
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].1.reason, reason::ABANDONED);
    let out = room.tick_sim(&mut sim);
    assert!(out.is_empty());
    let seen = &room.logic().game.s.seen;
    assert_eq!(seen[0], (Owner(0), Err(CallError::Abandoned)));
    assert_eq!(seen[1], (Owner(0), Err(CallError::Refused(Refusal::Gone))));
    let c = room.counters();
    assert_eq!(
        (c.results_abandoned, c.results_refused, c.calls_refused_gone),
        (2, 1, 1)
    );
    assert_eq!(
        c.responses_delivered, 2,
        "the .answers-only call did not go to the game"
    );
}

#[test]
fn a_refused_answers_call_answers_the_client_with_the_reason() {
    let mut sim = Sim::new();
    sim.service(Echo::serve(E), 1);
    let mut room = room(&sim, 64);
    let i = room.inputs();
    i.join(ALICE, "alice");
    room.logic_mut().game.s.answer_pings = true;
    i.request(ALICE, rpc(1, &Ping { n: 1 }));
    i.request(ALICE, rpc(2, &Ping { n: 2 }));
    room.tick_sim(&mut sim);
    let out = room.tick_sim(&mut sim);
    let by_id = |id| {
        out.iter()
            .find(|(_, r)| r.id == id)
            .map(|(_, r)| r.clone())
            .expect("answered")
    };
    assert_eq!(by_id(2).reason, reason::REFUSED_FULL);
    assert!(by_id(1).ok);
    assert_eq!(decode::<Pong>(&by_id(1)).n, 1);
}

#[test]
fn untaken_results_are_closed_and_counted() {
    let mut sim = Sim::new();
    sim.service(Echo::serve(E), 64);
    let mut room = room(&sim, 64);
    room.logic_mut().game.s.take = false;
    room.logic_mut().game.s.fire = 2;
    room.tick_sim(&mut sim);
    room.tick_sim(&mut sim); // arrives, nobody takes it
    assert_eq!(room.counters().responses_unhandled, 2);
    room.logic_mut().game.s.take = true;
    room.tick_sim(&mut sim);
    assert!(room.logic().game.s.seen.is_empty(), "closed, not kept");
}

#[test]
fn owner_gone_is_counted_and_not_delivered() {
    let mut sim = Sim::new();
    sim.service(Delay::ticks(2, Echo::serve(E)), 64);
    let mut room = room(&sim, 64);
    room.logic_mut().game.s.fire = 2;
    room.tick_sim(&mut sim);
    room.logic_mut().game.dead.push(Owner(1));
    room.tick_sim(&mut sim);
    room.tick_sim(&mut sim);
    assert_eq!(room.logic().game.s.seen, vec![(Owner(0), Ok(0))]);
    assert_eq!(room.counters().responses_owner_gone, 1);
}

// ---- requests ---------------------------------------------------------------

#[derive(Default)]
struct Q {
    mode: u8,
    kept: Option<ReplyTo<Ping>>,
}

fn qsys(s: &mut Q, calls: &mut Calls, _ctx: &mut TickCtx) {
    match s.mode {
        // Take and drop the reply token (a bug): closed at tick end.
        1 => {
            let _ = calls.take_incoming::<Ping>();
        }
        // Keep the token past the tick, answer next tick: too late.
        2 => {
            if let Some(r) = calls.take_incoming::<Ping>().pop() {
                s.kept = Some(r.reply);
            }
        }
        3 => {
            if let Some(t) = s.kept.take() {
                calls.reply(t, Ok(Pong { n: 0 }));
            }
        }
        // Don't take at all.
        _ => {}
    }
}

#[test]
fn unanswered_and_untaken_requests_are_closed_counted_and_answered() {
    let mut room = scripted(
        RoomConfig::default(),
        Q::default(),
        |c| c.accept::<Ping>().unwrap(),
        qsys,
    );
    let i = room.inputs();
    i.join(ALICE, "alice");

    i.request(ALICE, rpc(1, &Ping { n: 1 }));
    let out = room.tick(0.0);
    assert_eq!(out[0].1.reason, reason::UNTAKEN);

    room.logic_mut().game.s.mode = 1;
    i.request(ALICE, rpc(2, &Ping { n: 2 }));
    let out = room.tick(0.0);
    assert_eq!(out[0].1.reason, reason::UNANSWERED);

    room.logic_mut().game.s.mode = 2;
    i.request(ALICE, rpc(3, &Ping { n: 3 }));
    let out = room.tick(0.0);
    assert_eq!(out[0].1.reason, reason::UNANSWERED);
    room.logic_mut().game.s.mode = 3;
    let out = room.tick(0.0);
    assert!(out.is_empty(), "a late answer goes nowhere");

    let c = room.counters();
    assert_eq!(c.requests_untaken, 1);
    assert_eq!(c.requests_unanswered, 2);
    assert_eq!(c.requests_replied_late, 1);
}
