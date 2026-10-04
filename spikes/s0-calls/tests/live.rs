//! The live host: services on tokio behind the game's tower stack.
//!
//! - service-to-service through the awaitable `…Handle` (the verified
//!   player propagates with `within(cx)`);
//! - admin / boot code awaiting a `…Handle` directly;
//! - `timeout` and `concurrency_limit + load_shed` layers end calls as
//!   `TimedOut` / `Refused(Shed)` — through the same `take_resolved`.

mod common;

use std::time::{Duration, Instant};

use common::*;

#[service]
pub trait Shop {
    /// Buy a gift for `to`: the shop charges the buyer through the bank.
    async fn gift(&self, cx: &CallCx, to: Identity, price: u64) -> Result<TransferDone, GameError>;
    async fn slow(&self, cx: &CallCx, ms: u64) -> Result<u64, GameError>;
}

struct GiftShop {
    bank: BankHandle,
}

impl Shop for GiftShop {
    async fn gift(&self, cx: &CallCx, to: Identity, price: u64) -> Result<TransferDone, GameError> {
        // Service → service: awaitable, acting for the same player.
        self.bank
            .within(cx)
            .transfer(to, price)
            .await
            .map_err(|e| match e {
                CallError::Service(g) => g,
                _ => err(99),
            })
    }
    async fn slow(&self, _cx: &CallCx, ms: u64) -> Result<u64, GameError> {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(ms)
    }
}

#[derive(Default)]
struct S {
    shop: Option<ShopClient>,
    gift_for: Option<Identity>,
    slow: Vec<u64>,
    gifts: Vec<Result<u64, CallError<GameError>>>,
    slows: Vec<(Owner, Result<u64, CallError<GameError>>)>,
}

fn sys(s: &mut S, calls: &mut Calls, _ctx: &mut TickCtx) {
    for r in calls.take_resolved::<shop::Gift, ()>() {
        s.gifts.push(r.outcome.map(|d| d.balance));
    }
    for r in calls.take_resolved::<shop::Slow, ()>() {
        s.slows.push((r.owner, r.outcome));
    }
    let shop = s.shop.clone().expect("client");
    if let Some(who) = s.gift_for.take() {
        calls
            .call(shop.gift(Identity::new("bob"), 50))
            .on_behalf_of(&who)
            .owner(Owner(1));
    }
    for (i, ms) in std::mem::take(&mut s.slow).into_iter().enumerate() {
        calls.call(shop.slow(ms)).owner(Owner(i as u64));
    }
}

fn run_until(room: &mut ScriptedRoom<S>, done: impl Fn(&S) -> bool) {
    let start = Instant::now();
    while !done(&room.logic().game.s) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting"
        );
        room.tick(0.002);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap()
}

#[test]
fn a_service_calls_another_through_its_handle() {
    let rt = runtime();
    let be = FakeBackend::new();
    let mut server = Server::new(rt.handle().clone());
    server.service(Bank::serve(be.clone()), |s| {
        s.timeout(Duration::from_secs(3))
    });
    let bank: BankHandle = server.client();
    server.service(Shop::serve(GiftShop { bank: bank.clone() }), |s| s);

    let s = S {
        shop: Some(server.client()),
        gift_for: Some(Identity::new("alice")),
        ..S::default()
    };
    let mut room = scripted(RoomConfig::default(), s, |_| {}, sys);
    run_until(&mut room, |s| !s.gifts.is_empty());
    assert_eq!(room.logic().game.s.gifts, vec![Ok(450)]);
    assert_eq!(be.charges(), 1);

    // Admin / boot code: await a handle directly.
    let carol = rt.block_on(
        bank.on_behalf_of(&Identity::new("bob"))
            .transfer(Identity::new("carol"), 25),
    );
    assert_eq!(carol, Ok(TransferDone { balance: 125 }));
    assert_eq!(be.gold("carol"), 25);
}

#[test]
fn tower_layers_end_calls_through_the_same_path() {
    let rt = runtime();
    let be = FakeBackend::new();
    let mut server = Server::new(rt.handle().clone());
    server.service(Bank::serve(be), |s| s);
    let bank: BankHandle = server.client();
    let mut timed = Server::new(rt.handle().clone());
    timed.service(Shop::serve(GiftShop { bank: bank.clone() }), |s| {
        s.timeout(Duration::from_millis(20))
    });
    let mut shed = Server::new(rt.handle().clone());
    shed.service(Shop::serve(GiftShop { bank }), |s| {
        s.load_shed().concurrency_limit(1)
    });

    // timeout: 200 ms of work under a 20 ms timeout.
    let s = S {
        shop: Some(timed.client()),
        slow: vec![200],
        ..S::default()
    };
    let mut room = scripted(RoomConfig::default(), s, |_| {}, sys);
    run_until(&mut room, |s| !s.slows.is_empty());
    assert_eq!(
        room.logic().game.s.slows,
        vec![(Owner(0), Err(CallError::TimedOut))]
    );
    assert_eq!(room.counters().results_timed_out, 1);

    // load_shed over concurrency_limit(1): three at once, one runs.
    let s = S {
        shop: Some(shed.client()),
        slow: vec![50, 50, 50],
        ..S::default()
    };
    let mut room = scripted(RoomConfig::default(), s, |_| {}, sys);
    run_until(&mut room, |s| s.slows.len() == 3);
    let slows = &room.logic().game.s.slows;
    let ok = slows.iter().filter(|(_, r)| r.is_ok()).count();
    let shed_n = slows
        .iter()
        .filter(|(_, r)| *r == Err(CallError::Refused(Refusal::Shed)))
        .count();
    assert_eq!((ok, shed_n), (1, 2), "{slows:?}");
}
