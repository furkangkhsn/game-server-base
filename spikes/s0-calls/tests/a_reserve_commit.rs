//! (a) Five buy requests over five ticks, a service that answers three
//! ticks late → exactly one charge (reserve/commit in the world, the call
//! key as the idempotency key at the service).

mod common;

use common::*;
use s0_calls::fake::FakeService;
use s0_calls::request::ConnId;
use s0_calls::room::{Room, RoomConfig};

fn sword() -> BuyItem {
    BuyItem {
        item: "sword".into(),
        price: 500,
    }
}

#[test]
fn five_buys_over_five_ticks_charge_once() {
    let (econ, mut fake) = FakeService::new("economy", 8);
    let mut ledger = Ledger::with_balance(600);
    let mut room = Room::new(RoomConfig::default(), Shop::new(econ, 600), shop_table());
    let alice = ConnId(1);
    let (tx, mut out) = room.join(alice, 64);

    // One request per tick, fresh correlation ids (a client that keeps
    // clicking "buy"), while the service sits on the first one.
    for id in 1..=5 {
        tx.try_send(rpc_action(alice, id, &sword())).unwrap();
        room.tick();
        fake.step(room.now(), |m, s| ledger.handle(3, m, s));
    }
    for _ in 0..5 {
        room.tick();
        fake.step(room.now(), |m, s| ledger.handle(3, m, s));
    }

    assert_eq!(ledger.charges, 1, "exactly one charge at the service");
    assert_eq!(ledger.balance, 100);
    let shop = room.logic();
    assert_eq!(shop.balance, 100, "the commit updated the room's cache");
    assert_eq!(shop.reserved, 0, "nothing left reserved");
    assert_eq!(shop.keys.len(), 1, "only one call ever left the room");

    let got = responses(&mut out);
    assert_eq!(got.len(), 5, "every request answered exactly once");
    let oks: Vec<_> = got.iter().filter(|r| r.ok).collect();
    assert_eq!(oks.len(), 1);
    assert_eq!(oks[0].id, 1);
    assert_eq!(decode_ok(oks[0]).balance, 100);
    for r in got.iter().filter(|r| !r.ok) {
        assert_eq!(decode_err(r).code, code::INSUFFICIENT, "id {}", r.id);
    }

    let c = room.calls().counters();
    assert_eq!(c.calls_sent, 1);
    assert_eq!(c.calls_done_ok, 1);
    assert_eq!(room.calls().in_flight(), 0);
}

#[test]
fn a_retried_correlation_id_in_flight_is_refused_by_the_engine() {
    let (econ, mut fake) = FakeService::new("economy", 8);
    let mut ledger = Ledger::with_balance(600);
    let mut room = Room::new(RoomConfig::default(), Shop::new(econ, 600), shop_table());
    let alice = ConnId(1);
    let (tx, mut out) = room.join(alice, 64);

    // The same id five times (a client retrying its own request).
    for _ in 0..5 {
        tx.try_send(rpc_action(alice, 7, &sword())).unwrap();
        room.tick();
        fake.step(room.now(), |m, s| ledger.handle(3, m, s));
    }
    for _ in 0..5 {
        room.tick();
        fake.step(room.now(), |m, s| ledger.handle(3, m, s));
    }
    assert_eq!(ledger.charges, 1);
    assert_eq!(
        room.counters.requests_rejected_dup, 3,
        "ticks 2..4 hit the in-flight id"
    );
    let got = responses(&mut out);
    // 1 ok + 3 duplicates + the 5th retry (after the answer) is a fresh
    // request the shop rejects locally (insufficient).
    assert_eq!(got.len(), 5);
    assert_eq!(got.iter().filter(|r| r.ok).count(), 1);
}
