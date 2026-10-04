//! (c) A full service mailbox fails fast at the call site and is counted;
//! the per-(room, service) share keeps one room from starving the rest.

mod common;

use common::*;
use s0_calls::calls::{CallKey, Done, RefuseReason};
use s0_calls::fake::FakeService;
use s0_calls::room::{Logic, Room, RoomConfig, TickCtx};
use s0_calls::service::ServiceRef;

/// Issues `n` grants in SYSTEMS on its first tick and records each
/// call-site result as it happened.
struct Spammer {
    econ: ServiceRef<EconomyMsg>,
    n: usize,
    results: Vec<Result<CallKey, RefuseReason>>,
}

impl Logic for Spammer {
    type Done = ();
    type Req = ();
    fn on_done(&mut self, _ctx: &TickCtx<'_, ()>, _done: &mut Vec<Done<()>>) {}
    fn on_request(&mut self, _ctx: &TickCtx<'_, ()>, _req: ()) {}
    fn update(&mut self, ctx: &TickCtx<'_, ()>) {
        for _ in 0..std::mem::take(&mut self.n) {
            let r = ctx
                .call(&self.econ, GrantGold { amount: 1 }, |_| ())
                .map_err(|refused| refused.reason);
            self.results.push(r);
        }
    }
}

fn spammer(econ: ServiceRef<EconomyMsg>, n: usize, cap: u32) -> Room<Spammer> {
    let mut cfg = RoomConfig::default();
    cfg.calls.max_in_flight_per_service = cap;
    Room::new(
        cfg,
        Spammer {
            econ,
            n,
            results: Vec::new(),
        },
        Default::default(),
    )
}

#[test]
fn a_full_mailbox_is_refused_at_the_call_site_and_counted() {
    let (econ, _fake) = FakeService::<EconomyMsg>::new("economy", 2);
    let mut room = spammer(econ, 3, 32);
    room.tick();
    let r = &room.logic().results;
    assert!(r[0].is_ok() && r[1].is_ok());
    assert_eq!(r[2], Err(RefuseReason::Full), "known inside the same hook");
    let c = room.calls().counters();
    assert_eq!(c.calls_sent, 2);
    assert_eq!(c.calls_refused_full, 1);
    assert_eq!(
        room.calls().in_flight(),
        2,
        "a refusal leaves nothing pending"
    );
}

#[test]
fn a_gone_service_is_refused_and_counted() {
    let (econ, mut fake) = FakeService::<EconomyMsg>::new("economy", 2);
    fake.kill();
    let mut room = spammer(econ, 1, 32);
    room.tick();
    assert_eq!(room.logic().results[0], Err(RefuseReason::Gone));
    assert_eq!(room.calls().counters().calls_refused_gone, 1);
}

#[test]
fn one_room_cannot_take_more_than_its_share() {
    // A mailbox of 8 shared by two rooms, a share of 3 each.
    let (econ, _fake) = FakeService::<EconomyMsg>::new("economy", 8);
    let mut greedy = spammer(econ.clone(), 100, 3);
    let mut polite = spammer(econ, 3, 3);
    greedy.tick();
    polite.tick();
    let g = greedy.calls().counters();
    assert_eq!(g.calls_sent, 3);
    assert_eq!(g.calls_refused_service_cap, 97);
    let p = polite.calls().counters();
    assert_eq!(p.calls_sent, 3, "the second room still got its share");
    assert_eq!(p.calls_refused_full + p.calls_refused_service_cap, 0);
}

#[test]
fn a_refused_purchase_rolls_back_its_reserve_through_the_same_path() {
    let (econ, _fake) = FakeService::new("economy", 1);
    let mut room = Room::new(RoomConfig::default(), Shop::new(econ, 1000), shop_table());
    let alice = s0_calls::request::ConnId(1);
    let (tx, mut out) = room.join(alice, 64);
    let buy = BuyItem {
        item: "potion".into(),
        price: 100,
    };
    tx.try_send(rpc_action(alice, 1, &buy)).unwrap();
    tx.try_send(rpc_action(alice, 2, &buy)).unwrap();
    room.tick();
    // First call took the only mailbox slot; the second was refused,
    // `Refused::fail` ran the tag, the shop released its reserve and
    // answered BUSY in the same tick.
    assert_eq!(room.logic().reserved, 100);
    let got = responses(&mut out);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, 2);
    assert_eq!(decode_err(&got[0]).code, code::BUSY);
}
