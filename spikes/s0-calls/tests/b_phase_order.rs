//! (b) RESPONSES before READ: a call's answer is absorbed before the
//! same tick's input is converted, and EVENTS sit between them.

mod common;

use common::*;
use s0_calls::fake::FakeService;
use s0_calls::push::{Dest, Directory, Push, SessionRoute};
use s0_calls::request::ConnId;
use s0_calls::room::{Room, RoomConfig};

#[test]
fn a_response_is_absorbed_before_the_same_ticks_request() {
    let (econ, mut fake) = FakeService::new("economy", 8);
    let mut ledger = Ledger::with_balance(0);
    let mut shop = Shop::new(econ, 0);
    shop.grant_on_update = Some(500);
    let mut room = Room::new(RoomConfig::default(), shop, shop_table());
    let alice = ConnId(1);
    let (tx, mut out) = room.join(alice, 64);

    room.tick(); // t1: SYSTEMS calls GrantGold
    fake.step(room.now(), |m, s| ledger.handle(1, m, s)); // answer at t2
    room.tick(); // t2
    fake.step(room.now(), |m, s| ledger.handle(1, m, s)); // grant answered

    // The grant's answer is waiting in the completion channel; the buy
    // request is waiting in the action channel. Same tick window.
    tx.try_send(rpc_action(
        alice,
        1,
        &BuyItem {
            item: "sword".into(),
            price: 500,
        },
    ))
    .unwrap();
    room.tick(); // t3
    assert_eq!(room.logic().trace_at(3), vec!["done", "request", "update"]);
    fake.step(room.now(), |m, s| ledger.handle(0, m, s));
    room.tick(); // t4: the buy's answer

    let got = responses(&mut out);
    assert_eq!(got.len(), 1);
    // Validated against the balance the grant just set: accepted.
    assert_eq!(decode_ok(&got[0]).balance, 0);
    assert_eq!(ledger.charges, 1);
}

#[test]
fn the_full_order_is_done_events_ingest_request_update() {
    let (econ, mut fake) = FakeService::new("economy", 8);
    let mut ledger = Ledger::with_balance(0);
    let mut shop = Shop::new(econ, 0);
    shop.grant_on_update = Some(10);
    let mut room = Room::new(RoomConfig::default(), shop, shop_table());
    let alice = ConnId(1);
    let (tx, mut client) = room.join(alice, 64);
    let mut dir = Directory::default();
    let (client_out, _client_rx) = tokio::sync::mpsc::channel(8);
    dir.bind(
        "alice",
        SessionRoute {
            conn: alice,
            push_capable: true,
            client_out,
            room_in: Some(tx.clone()),
        },
    );

    room.tick(); // t1: grant call
    fake.step(room.now(), |m, s| ledger.handle(0, m, s)); // answered now

    // Everything lands in the same window: an answer, a world push, a
    // plain action and a request.
    dir.route(Push::new(
        "alice",
        Dest::World,
        &BalanceChanged { balance: 900 },
    ));
    tx.try_send(plain_action(alice)).unwrap();
    tx.try_send(rpc_action(
        alice,
        1,
        &BuyItem {
            item: "potion".into(),
            price: 900,
        },
    ))
    .unwrap();
    room.tick(); // t2
    assert_eq!(
        room.logic().trace_at(2),
        vec!["done", "events", "ingest", "request", "update"]
    );
    // The request saw the pushed balance (EVENTS ran before CONVERT).
    assert_eq!(room.logic().reserved, 900);
    assert!(responses(&mut client).is_empty(), "still in flight");
}
