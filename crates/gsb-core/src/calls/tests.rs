//! Table-level tests (no actor): what is easiest to drive between two
//! tables — migration (`export`/`import`, spike `migration`) — and the
//! builder's "who gets the result" rule.

use std::collections::BTreeMap;

use prost::Message;

use super::*;
use crate::id::{ConnectionId, PlayerId};
use crate::room::RoomCounters;

#[derive(Clone, PartialEq, prost::Message)]
struct Pay {
    #[prost(uint64, tag = "1")]
    amount: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
struct Paid {
    #[prost(uint64, tag = "1")]
    balance: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
struct Nope {
    #[prost(uint32, tag = "1")]
    code: u32,
}
impl Call for Pay {
    type Response = Paid;
    type Error = Nope;
}
impl Request for Pay {
    const OP: u16 = 1;
}

/// The bank's call.
#[derive(Clone, Debug)]
struct Charge(u64);
impl Call for Charge {
    type Response = Paid;
    type Error = Nope;
}
enum BankMsg {
    Charge(Ask<Charge>),
}

/// An idempotent bank: one effect per key, the stored answer for a
/// repeated key.
#[derive(Default)]
struct Ledger {
    balance: Option<u64>,
    done: BTreeMap<CallKey, Paid>,
    charges: u32,
    keys: Vec<CallKey>,
}

impl Ledger {
    fn serve(&mut self, inbox: &mut crate::channel::Inbox<Mail<BankMsg>>) {
        while let Ok(Mail::Call(Envelope { cx, msg })) = inbox.try_recv() {
            let BankMsg::Charge(ask) = msg;
            self.keys.push(cx.key());
            let paid = match self.done.get(&cx.key()) {
                Some(p) => p.clone(),
                None => {
                    let b = self.balance.get_or_insert(500);
                    *b -= ask.call.0;
                    self.charges += 1;
                    let p = Paid { balance: *b };
                    self.done.insert(cx.key(), p.clone());
                    p
                }
            };
            ask.answer(Ok(paid));
        }
    }
}

fn table(origin: u64) -> Calls {
    let mut calls = Calls::new(CallsConfig {
        actor: ActorId(origin),
        origin,
        ..CallsConfig::default()
    });
    calls.accept::<Pay>().expect("boot");
    calls
}

fn alive(_: Owner) -> bool {
    true
}

/// A charges, the bank charges; A exports the owner (its call, its
/// context, its `.answers` target, its bound session) and B imports it
/// under a new owner; B re-issues under the SAME key — the bank answers
/// from its ledger: ONE charge. B gets the result with its context and
/// answers the client; A's late answer is applied nowhere.
#[test]
fn an_exported_call_is_reissued_under_its_key_one_effect() {
    let (bank, mut inbox) = Route::<BankMsg>::new("bank", 8);
    let mut ledger = Ledger::default();
    let mut m = RoomCounters::default();
    let mut a = table(1);
    let mut b = table(2);
    let conn = ConnectionId(3);
    a.bind(conn, Owner(5));

    let pay = Pay { amount: 100 }.encode_to_vec();
    a.read(
        conn,
        PlayerId(5),
        &Identity::new("al"),
        None,
        77,
        Pay::OP,
        &pay,
    );
    let r = a.take_incoming::<Pay>().pop().expect("taken");
    assert_eq!(r.owner, Owner(5), "the explicit binding");
    a.call(CallSpec::new(&bank, Charge(r.body.amount), BankMsg::Charge))
        .owner(r.owner)
        .with(100u64)
        .answers(r.reply);
    a.flush();
    a.end_tick();
    ledger.serve(&mut inbox);
    assert_eq!(ledger.charges, 1);

    let moved = a.export(Owner(5));
    assert_eq!(moved.len(), 1);
    assert_eq!(moved.keys(), [Some(ledger.keys[0])]);
    assert_eq!(a.bound(conn), None, "the session's binding travels");
    assert!(!a.is_live(conn, 77), "the source owes no answer any more");

    b.import(moved, Owner(9));
    assert_eq!(b.bound(conn), Some(Owner(9)));
    assert!(b.in_flight::<Charge>(Owner(9)));
    b.flush();
    ledger.serve(&mut inbox);
    assert_eq!(ledger.keys, [ledger.keys[0]; 2], "the same key twice");
    assert_eq!(ledger.charges, 1, "one effect");

    a.responses(std::time::Instant::now(), &alive, &mut m);
    assert_eq!(a.counts().results_late, 1, "A's answer is applied nowhere");
    assert!(a.take_outbox().is_empty());

    b.responses(std::time::Instant::now(), &alive, &mut m);
    let got = b.take_resolved::<Charge, u64>();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].owner, got[0].ctx), (Owner(9), 100));
    assert_eq!(got[0].outcome, Ok(Paid { balance: 400 }));
    let out = b.take_outbox();
    assert_eq!(out.len(), 1, "B answers the client");
    assert_eq!((out[0].0, out[0].1.id, out[0].1.ok), (conn, 77, true));
    let c = *a.counts();
    assert_eq!((c.calls_issued, c.calls_exported), (1, 1));
    let c = *b.counts();
    assert_eq!((c.calls_imported, c.calls_issued, c.results_ok), (1, 1, 1));
}

/// Who gets the result (SERVICES.md §5.2): `.answers` alone → only the
/// client; `.owner`/`.with` (or no `.answers`) → the game too.
#[test]
fn an_answers_only_call_answers_the_client_and_nobody_else() {
    let (bank, mut inbox) = Route::<BankMsg>::new("bank", 8);
    let mut ledger = Ledger::default();
    let mut m = RoomCounters::default();
    let mut t = table(1);
    let pay = Pay { amount: 1 }.encode_to_vec();
    let who = Identity::new("p");
    t.read(
        ConnectionId(1),
        PlayerId(1),
        &who,
        Some(Owner(1)),
        1,
        Pay::OP,
        &pay,
    );
    t.read(
        ConnectionId(1),
        PlayerId(1),
        &who,
        Some(Owner(1)),
        2,
        Pay::OP,
        &pay,
    );
    let mut rs = t.take_incoming::<Pay>().into_iter();
    let (r1, r2) = (rs.next().expect("1"), rs.next().expect("2"));
    t.call(CallSpec::new(&bank, Charge(1), BankMsg::Charge))
        .answers(r1.reply);
    t.call(CallSpec::new(&bank, Charge(1), BankMsg::Charge))
        .owner(r2.owner)
        .answers(r2.reply);
    t.call(CallSpec::new(&bank, Charge(1), BankMsg::Charge));
    t.flush();
    t.end_tick();
    ledger.serve(&mut inbox);
    t.responses(std::time::Instant::now(), &alive, &mut m);
    assert_eq!(t.take_outbox().len(), 2, "two clients answered");
    let got = t.take_resolved::<Charge, ()>();
    let owners: Vec<Owner> = got.iter().map(|r| r.owner).collect();
    assert_eq!(owners, [Owner(1), Owner::ACTOR], "no answers-only result");
    let c = *t.counts();
    assert_eq!((c.answers_sent, c.responses_delivered), (2, 2));
    t.end_tick();
    assert_eq!(t.counts().responses_unhandled, 0);
}

/// The answer shapes (`docs/SERVICES.md` §16): ok; a typed game error —
/// even a default-valued one that encodes to ZERO bytes — flagged
/// `game_error` with an empty reason; every engine ending unflagged with
/// its reason.
#[test]
fn typed_outcomes_encode_to_the_flagged_answer_shapes() {
    use crate::calls::table::encode_answer;
    let enc = |o: Outcome<Pay>| encode_answer::<Pay>(&(Box::new(o) as contract::Erased), 9, 1);
    let ok = enc(Ok(Paid { balance: 3 }));
    assert!(ok.ok && !ok.game_error && ok.payload[..] == Paid { balance: 3 }.encode_to_vec()[..]);
    let zero = enc(Err(CallError::Service(Nope::default())));
    assert!(!zero.ok && zero.game_error && zero.payload.is_empty() && zero.reason.is_empty());
    let coded = enc(Err(CallError::Service(Nope { code: 4 })));
    assert!(coded.game_error && Nope::decode(&coded.payload[..]).unwrap().code == 4);
    for (o, why) in [
        (Err(CallError::TimedOut), crate::rpc::TIMEOUT_REASON),
        (Err(CallError::Abandoned), crate::rpc::ABANDONED_REASON),
        (
            Err(CallError::Refused(Refusal::Gone)),
            crate::rpc::REFUSED_GONE_REASON,
        ),
    ] {
        let r = enc(o);
        assert!(!r.ok && !r.game_error && r.reason == why && r.payload.is_empty());
        assert_eq!((r.id, r.op), (9, 1));
    }
}

/// The table's own deadline (SERVICES.md §19 S6, B154): a sent call the
/// service has not answered by `CallsConfig::timeout` ends `TimedOut` at
/// RESPONSES — its client answered with the timeout reason, its result
/// delivered, counted `results_timed_out` — and the service's later
/// answer is `results_late`, applied nowhere.
#[test]
fn a_call_past_the_tables_deadline_ends_timed_out_and_its_answer_is_late() {
    let (bank, mut inbox) = Route::<BankMsg>::new("bank", 8);
    let mut ledger = Ledger::default();
    let mut m = RoomCounters::default();
    let mut t = Calls::new(CallsConfig {
        timeout: Some(std::time::Duration::ZERO),
        ..CallsConfig::default()
    });
    t.accept::<Pay>().expect("boot");
    let pay = Pay { amount: 1 }.encode_to_vec();
    t.read(
        ConnectionId(1),
        PlayerId(1),
        &Identity::new("p"),
        Some(Owner(1)),
        9,
        Pay::OP,
        &pay,
    );
    let r = t.take_incoming::<Pay>().pop().expect("taken");
    t.call(CallSpec::new(&bank, Charge(1), BankMsg::Charge))
        .owner(r.owner)
        .with(7u8)
        .answers(r.reply);
    t.flush();
    t.end_tick();
    t.responses(crate::ticker::now(), &alive, &mut m);
    let got = t.take_resolved::<Charge, u8>();
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].ctx, &got[0].outcome),
        (7, &Err(CallError::TimedOut))
    );
    let out = t.take_outbox();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].1.id, out[0].1.ok, out[0].1.reason.as_str()),
        (9, false, crate::rpc::TIMEOUT_REASON)
    );
    assert!(!t.in_flight::<Charge>(Owner(1)));
    assert_eq!(t.pending_len(), 0);
    // The service answers after all: late, applied nowhere.
    ledger.serve(&mut inbox);
    t.responses(crate::ticker::now(), &alive, &mut m);
    assert!(t.take_resolved::<Charge, u8>().is_empty());
    assert!(t.take_outbox().is_empty());
    let c = *t.counts();
    assert_eq!(
        (
            c.results_timed_out,
            c.results_ok,
            c.results_late,
            c.answers_sent
        ),
        (1, 0, 1, 1)
    );
}

/// No deadline (`timeout: None`): a call waits for its answer however
/// long it takes.
#[test]
fn without_a_deadline_a_call_waits_for_its_answer() {
    let (bank, mut inbox) = Route::<BankMsg>::new("bank", 8);
    let mut ledger = Ledger::default();
    let mut m = RoomCounters::default();
    let mut t = Calls::new(CallsConfig {
        timeout: None,
        ..CallsConfig::default()
    });
    t.call(CallSpec::new(&bank, Charge(1), BankMsg::Charge));
    t.flush();
    t.responses(
        crate::ticker::now() + std::time::Duration::from_secs(3600),
        &alive,
        &mut m,
    );
    assert_eq!(t.pending_len(), 1);
    ledger.serve(&mut inbox);
    t.responses(crate::ticker::now(), &alive, &mut m);
    assert_eq!(t.take_resolved::<Charge, ()>().len(), 1);
    assert_eq!(t.counts().results_timed_out, 0);
}
