//! Migration with a call in flight: the engine's pending entry does NOT
//! travel (dropped and counted at the source; the source's later answer
//! is `calls_late`); the game's reserve carries the key, the destination
//! re-issues under it, and the service's key dedupe keeps it to one
//! charge. The same recipe serves a supervision restart.

mod common;

use common::*;
use s0_calls::fake::FakeService;
use s0_calls::request::ConnId;
use s0_calls::room::{Room, RoomConfig};

#[test]
fn a_reissue_under_the_same_key_charges_once() {
    let (econ, mut fake) = FakeService::new("economy", 8);
    let mut ledger = Ledger::with_balance(1000);
    let cfg_a = RoomConfig {
        origin: 1,
        ..RoomConfig::default()
    };
    let cfg_b = RoomConfig {
        origin: 2,
        ..RoomConfig::default()
    };
    let mut a = Room::new(cfg_a, Shop::new(econ.clone(), 1000), shop_table());
    let mut b = Room::new(cfg_b, Shop::new(econ, 1000), shop_table());
    let alice = ConnId(1);
    let (tx, _out) = a.join(alice, 8);
    let buy = BuyItem {
        item: "mount".into(),
        price: 400,
    };

    tx.try_send(rpc_action(alice, 1, &buy)).unwrap();
    a.tick();
    fake.step(a.now(), |m, s| ledger.handle(5, m, s)); // charged now, answer at 6
    assert_eq!(ledger.charges, 1);
    let key = a.logic().keys[0];

    // Migrate-out: the engine forgets the call here...
    a.calls().drop_for_migration(&[key]);
    // ...the game's reserve (with its key) travels in its migration state.
    b.logic_mut().reissue.push((key, buy));

    for _ in 0..8 {
        a.tick();
        b.tick();
        fake.step(b.now(), |m, s| ledger.handle(5, m, s));
    }

    assert_eq!(ledger.charges, 1, "the re-issue was deduped by key");
    assert_eq!(ledger.keys_seen, 1);
    assert_eq!(b.logic().settled, vec![Ok(600)]);
    assert_eq!(b.logic().reserved, 0);
    let ca = a.calls().counters();
    assert_eq!(ca.calls_dropped_migrated, 1);
    assert_eq!(
        ca.calls_late, 1,
        "the source's answer landed after the move"
    );
    assert_eq!(b.calls().counters().calls_done_ok, 1);
}

#[test]
fn teardown_counts_what_was_in_flight() {
    let (econ, _fake) = FakeService::new("economy", 8);
    let mut shop = Shop::new(econ, 0);
    shop.grant_on_update = Some(1);
    let mut room = Room::new(RoomConfig::default(), shop, shop_table());
    room.tick();
    room.calls().teardown();
    assert_eq!(room.calls().counters().calls_lost_teardown, 1);
}
