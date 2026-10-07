//! Migration with a call in flight: `export(owner)` carries the call's
//! key, context and client answer out of room A; `import` in room B
//! re-issues it under the SAME key (the session's owner binding moves
//! with it); the idempotent service charges once.
//! A's late answer is counted (`results_late`); B gets the result with
//! the context, and the client is answered from B.

mod common;

use common::*;
use s0_calls::room::FIXED_DT;

#[derive(Default)]
struct S {
    bank: Option<BankClient>,
    seen: Vec<(Owner, u64, Result<u64, CallError<GameError>>)>,
}

fn sys(s: &mut S, calls: &mut Calls, _ctx: &mut TickCtx) {
    for r in calls.take_resolved::<bank::Transfer, Reserve>() {
        s.seen
            .push((r.owner, r.ctx.0, r.outcome.map(|d| d.balance)));
    }
    let bank = s.bank.clone().expect("client");
    for r in calls.take_incoming::<TransferGold>() {
        calls
            .call(bank.transfer(Identity::new(&r.body.to), r.body.amount))
            .owner(r.owner)
            .with(Reserve(r.body.amount))
            .answers(r.reply);
    }
}

#[test]
fn export_import_reissues_under_the_same_key_and_charges_once() {
    let be = FakeBackend::new();
    let mut sim = Sim::new();
    sim.service(Delay::ticks(3, Bank::serve(be.clone())), 8);
    let mk = |id: u64| {
        let s = S {
            bank: Some(sim.client()),
            seen: Vec::new(),
        };
        scripted(
            RoomConfig {
                id,
                ..RoomConfig::default()
            },
            s,
            |c| c.accept::<TransferGold>().unwrap(),
            sys,
        )
    };
    let (mut a, mut b) = (mk(1), mk(2));
    a.inputs().join(ALICE, "alice");
    a.inputs().request(
        ALICE,
        rpc(
            10,
            &TransferGold {
                to: "bob".into(),
                amount: 100,
            },
        ),
    );

    let mut out_a = Vec::new();
    let mut out_b = Vec::new();
    let mut step = |a: &mut ScriptedRoom<S>, b: &mut ScriptedRoom<S>, sim: &mut Sim, t: u64| {
        out_a.extend(a.tick(FIXED_DT));
        out_b.extend(b.tick(FIXED_DT));
        sim.step(t);
    };
    step(&mut a, &mut b, &mut sim, 0); // A issues; the bank charges now, answers at t3
    assert_eq!(be.charges(), 1);

    // The player (and their pending transfer) moves to B.
    let alice = a.logic().calls.bound(ALICE).expect("bound at join");
    let exported = a.logic_mut().calls.export(alice);
    assert_eq!(exported.len(), 1);
    let key = exported.keys()[0].expect("sent calls carry their key");
    assert!(!a.logic().calls.in_flight::<bank::Transfer>(alice));
    assert_eq!(
        a.logic().calls.bound(ALICE),
        None,
        "the binding left with the export"
    );
    b.logic_mut().calls.import(exported, Owner(9));
    assert_eq!(
        b.logic().calls.bound(ALICE),
        Some(Owner(9)),
        "and arrived with the import"
    );
    assert!(b.logic().calls.in_flight::<bank::Transfer>(Owner(9)));

    for t in 1..6 {
        step(&mut a, &mut b, &mut sim, t);
    }

    assert_eq!(
        be.charges(),
        1,
        "the re-issue under the same key was deduped"
    );
    assert_eq!(be.gold("alice"), 400);
    assert_eq!(b.logic().game.s.seen, vec![(Owner(9), 100, Ok(400))]);
    assert!(a.logic().game.s.seen.is_empty());
    assert!(out_a.is_empty(), "A no longer answers the client");
    assert_eq!(out_b.len(), 1, "B answers it");
    assert_eq!(out_b[0].0, ALICE);
    assert_eq!(out_b[0].1.id, 10);
    assert_eq!(decode::<TransferDone>(&out_b[0].1).balance, 400);

    let (ca, cb) = (a.counters(), b.counters());
    assert_eq!((ca.calls_exported, ca.results_late), (1, 1));
    assert_eq!((cb.calls_imported, cb.results_ok), (1, 1));
    assert_ne!(
        key.origin, 0,
        "keys are derived from the room's recorded seed"
    );
}
