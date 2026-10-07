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
