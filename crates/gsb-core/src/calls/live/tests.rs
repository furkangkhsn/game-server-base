//! The live tower host end to end: a `#[gsb_core::service]` trait served
//! by [`spawn`]/[`Services`], called from a [`Calls`] table (FLUSH →
//! service → RESPONSES) and through the awaitable `…Handle`; the layer
//! endings mapped and counted; the table's own deadline sweep; the stop.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::*;
use crate::calls::{
    ActorId, CallError, CallKey, Caller, Calls, CallsConfig, Identity, Outcome, Owner, Refusal,
};
use crate::room::RoomCounters;

#[crate::service]
pub trait Bank {
    /// Charge the caller.
    async fn charge(&self, cx: &CallCx, amount: u64) -> Result<u64, String>;
    /// Who called, and in which trace.
    async fn whoami(&self, cx: &CallCx) -> Result<(Option<Identity>, u64), String>;
    /// Sleep, then answer.
    async fn slow(&self, cx: &CallCx, ms: u64) -> Result<(), String>;
    /// Never answers.
    async fn hang(&self, cx: &CallCx) -> Result<(), String>;
    /// Panics.
    async fn boom(&self, cx: &CallCx) -> Result<(), String>;
}

#[derive(Default)]
struct Vault {
    running: AtomicU64,
    peak: AtomicU64,
}

impl Bank for Vault {
    async fn charge(&self, _cx: &CallCx, amount: u64) -> Result<u64, String> {
        if amount > 500 {
            return Err("insufficient funds".into());
        }
        Ok(500 - amount)
    }
    async fn whoami(&self, cx: &CallCx) -> Result<(Option<Identity>, u64), String> {
        Ok((cx.player().cloned(), cx.trace_id()))
    }
    async fn slow(&self, _cx: &CallCx, ms: u64) -> Result<(), String> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
    async fn hang(&self, _cx: &CallCx) -> Result<(), String> {
        std::future::pending().await
    }
    async fn boom(&self, _cx: &CallCx) -> Result<(), String> {
        panic!("the vault exploded (expected by the test)")
    }
}

/// A service that calls the bank on behalf of its own caller.
#[crate::service]
pub trait Shop {
    async fn ask_bank(&self, cx: &CallCx) -> Result<(Option<Identity>, u64), String>;
}

struct Till {
    bank: BankHandle,
}

impl Shop for Till {
    async fn ask_bank(&self, cx: &CallCx) -> Result<(Option<Identity>, u64), String> {
        self.bank
            .within(cx)
            .whoami()
            .await
            .map_err(|e| format!("{e:?}"))
    }
}

fn table(timeout: Option<Duration>) -> Calls {
    Calls::new(CallsConfig {
        actor: ActorId(7),
        origin: 7,
        timeout,
        ..CallsConfig::default()
    })
}

fn alive(_: Owner) -> bool {
    true
}

/// RESPONSES until `calls` holds no call in flight (bounded).
async fn settle(calls: &mut Calls, m: &mut RoomCounters) {
    for _ in 0..2000 {
        calls.responses(crate::ticker::now(), &alive, m);
        if calls.pending_len() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("calls still in flight: {}", calls.pending_len());
}

#[tokio::test]
async fn a_table_call_reaches_the_service_and_comes_back_at_responses() {
    let mut services = Services::new();
    services
        .service(Bank::serve(Vault::default()), |s| s)
        .expect("registered once");
    let bank: BankClient = services.client();
    let mut calls = table(None);
    let mut m = RoomCounters::default();
    calls.call(bank.charge(100)).with("a");
    calls.call(bank.charge(900)).with("b");
    calls.flush();
    settle(&mut calls, &mut m).await;
    let mut got: Vec<_> = calls
        .take_resolved::<bank::Charge, &str>()
        .into_iter()
        .map(|r| (r.ctx, r.outcome))
        .collect();
    got.sort_by_key(|(c, _)| *c);
    assert_eq!(
        got,
        vec![
            ("a", Ok(400)),
            (
                "b",
                Err(CallError::Service("insufficient funds".to_string()))
            )
        ]
    );
    let c = calls.counts();
    assert_eq!(
        (c.calls_sent, c.results_ok, c.results_service_error),
        (2, 1, 1)
    );
    let stats = &services.stats()[0];
    assert_eq!(stats.0, "Bank");
    assert_eq!(stats.1.calls_received.load(Ordering::Relaxed), 2);
    // Per-method stats: `charge` is method 0.
    assert_eq!(stats.1.methods[0].name, "charge");
    assert_eq!(stats.1.methods[0].calls.load(Ordering::Relaxed), 2);
    assert_eq!(stats.1.methods[1].calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn a_handle_awaits_and_carries_the_player() {
    let (route, _stop) = spawn(Bank::serve(Vault::default()), |s| s);
    let h = BankHandle::from_route(route);
    assert_eq!(h.charge(1).await, Ok(499));
    let bob = Identity::new("bob");
    let (who, _) = h.on_behalf_of(&bob).whoami().await.expect("answer");
    assert_eq!(who, Some(bob));
    let (who, _) = h.whoami().await.expect("answer");
    assert_eq!(who, None);
}

#[tokio::test]
async fn a_service_calls_another_within_its_callers_context() {
    let mut services = Services::new();
    services
        .service(Bank::serve(Vault::default()), |s| s)
        .expect("registered once");
    let till = Till {
        bank: services.client(),
    };
    services
        .service(Shop::serve(till), |s| s)
        .expect("registered once");
    let shop: ShopClient = services.client();
    let mut calls = table(None);
    let mut m = RoomCounters::default();
    let ann = Identity::new("ann");
    calls.call(shop.ask_bank()).on_behalf_of(&ann).with(());
    calls.flush();
    settle(&mut calls, &mut m).await;
    let r = calls
        .take_resolved::<shop::AskBank, ()>()
        .pop()
        .expect("one");
    let (who, trace) = r.outcome.expect("answer");
    // The bank saw the room's player and the room call's trace.
    assert_eq!(who, Some(ann));
    let room_cx = CallCx::new(
        r.key,
        Caller {
            actor: ActorId(7),
            player: None,
        },
    );
    assert_eq!(trace, room_cx.trace_id());
}

#[tokio::test]
async fn the_timeout_layer_ends_a_call_timed_out_and_counts_it() {
    let mut services = Services::new();
    services
        .service(Bank::serve(Vault::default()), |s| {
            s.timeout(Duration::from_millis(20))
        })
        .expect("registered once");
    let bank: BankClient = services.client();
    let mut calls = table(None);
    let mut m = RoomCounters::default();
    calls.call(bank.slow(2000)).with(());
    calls.flush();
    settle(&mut calls, &mut m).await;
    let r = calls.take_resolved::<bank::Slow, ()>().pop().expect("one");
    assert_eq!(r.outcome, Err(CallError::TimedOut));
    assert_eq!(calls.counts().results_timed_out, 1);
    assert_eq!(
        services.stats()[0]
            .1
            .calls_timed_out
            .load(Ordering::Relaxed),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn the_default_deadline_ends_a_call_that_never_answers() {
    let (route, _stop) = spawn(Bank::serve(Vault::default()), |s| s);
    let h = BankHandle::from_route(route.clone());
    let started = tokio::time::Instant::now();
    // Bounded on the (paused) clock: a host that lost its deadline fails
    // here instead of hanging the test run.
    let ended = tokio::time::timeout(DEFAULT_SERVICE_TIMEOUT * 2, h.hang()).await;
    assert_eq!(
        ended,
        Ok(Err(CallError::TimedOut)),
        "the host's default deadline must end the call"
    );
    assert!(started.elapsed() >= DEFAULT_SERVICE_TIMEOUT);
    assert_eq!(route.stats().calls_timed_out.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn load_shed_over_a_concurrency_limit_refuses_shed() {
    let vault = Arc::new(Vault::default());
    struct Shared(Arc<Vault>);
    impl Bank for Shared {
        async fn charge(&self, cx: &CallCx, amount: u64) -> Result<u64, String> {
            self.0.charge(cx, amount).await
        }
        async fn whoami(&self, cx: &CallCx) -> Result<(Option<Identity>, u64), String> {
            self.0.whoami(cx).await
        }
        async fn slow(&self, cx: &CallCx, ms: u64) -> Result<(), String> {
            self.0.slow(cx, ms).await
        }
        async fn hang(&self, cx: &CallCx) -> Result<(), String> {
            self.0.hang(cx).await
        }
        async fn boom(&self, cx: &CallCx) -> Result<(), String> {
            self.0.boom(cx).await
        }
    }
    let mut services = Services::new();
    services
        .service(Bank::serve(Shared(Arc::clone(&vault))), |s| {
            s.load_shed().concurrency_limit(1)
        })
        .expect("registered once");
    let bank: BankClient = services.client();
    let mut calls = table(None);
    let mut m = RoomCounters::default();
    calls.call(bank.slow(200)).with(1u8);
    calls.flush();
    // The first holds the only slot; then the second arrives.
    tokio::time::sleep(Duration::from_millis(50)).await;
    calls.call(bank.slow(1)).with(2u8);
    calls.flush();
    settle(&mut calls, &mut m).await;
    let mut got: Vec<_> = calls
        .take_resolved::<bank::Slow, u8>()
        .into_iter()
        .map(|r| (r.ctx, r.outcome))
        .collect();
    got.sort_by_key(|(c, _)| *c);
    assert_eq!(
        got,
        vec![(1, Ok(())), (2, Err(CallError::Refused(Refusal::Shed)))]
    );
    assert_eq!(calls.counts().results_refused, 1);
    assert_eq!(services.stats()[0].1.calls_shed.load(Ordering::Relaxed), 1);
    assert_eq!(vault.peak.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_concurrency_limit_queues_instead_of_running_in_parallel() {
    let mut services = Services::new();
    services
        .service(Bank::serve(Vault::default()), |s| s.concurrency_limit(1))
        .expect("registered once");
    let h: BankHandle = services.client();
    let a = tokio::spawn({
        let h = h.clone();
        async move { h.slow(30).await }
    });
    let b = tokio::spawn({
        let h = h.clone();
        async move { h.slow(30).await }
    });
    assert_eq!(a.await.expect("a"), Ok(()));
    assert_eq!(b.await.expect("b"), Ok(()));
    assert_eq!(services.stats()[0].1.calls_shed.load(Ordering::Relaxed), 0);
    assert_eq!(
        services.stats()[0].1.methods[2]
            .calls
            .load(Ordering::Relaxed),
        2
    );
}

#[tokio::test]
async fn a_panicking_service_abandons_the_call_and_counts_it() {
    let (route, _stop) = spawn(Bank::serve(Vault::default()), |s| s);
    let h = BankHandle::from_route(route.clone());
    assert_eq!(h.boom().await, Err(CallError::Abandoned));
    // The counter is bumped as the task unwinds, after the answer.
    for _ in 0..200 {
        if route.stats().calls_abandoned.load(Ordering::Relaxed) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("calls_abandoned not counted");
}

#[tokio::test(start_paused = true)]
async fn the_tables_own_deadline_ends_a_call_the_service_never_answers() {
    let (route, _stop) = spawn(Bank::serve(Vault::default()), |s| s.no_timeout());
    let bank = BankClient::from_route(route);
    let mut calls = table(Some(Duration::from_secs(5)));
    let mut m = RoomCounters::default();
    calls.call(bank.hang()).with(());
    calls.flush();
    tokio::time::sleep(Duration::from_secs(4)).await;
    calls.responses(crate::ticker::now(), &alive, &mut m);
    assert_eq!(calls.pending_len(), 1, "not due yet");
    tokio::time::sleep(Duration::from_secs(2)).await;
    calls.responses(crate::ticker::now(), &alive, &mut m);
    assert_eq!(calls.pending_len(), 0);
    let r = calls
        .take_resolved::<bank::Hang, ()>()
        .pop()
        .expect("swept");
    assert_eq!(r.outcome, Err(CallError::TimedOut));
    assert_eq!(calls.counts().results_timed_out, 1);
    assert!(!calls.in_flight::<bank::Hang>(Owner::ACTOR));
}

#[tokio::test(start_paused = true)]
async fn an_answer_after_the_tables_deadline_is_late() {
    let (route, _stop) = spawn(Bank::serve(Vault::default()), |s| s.no_timeout());
    let bank = BankClient::from_route(route);
    let mut calls = table(Some(Duration::from_millis(100)));
    let mut m = RoomCounters::default();
    calls.call(bank.slow(500)).with(());
    calls.flush();
    tokio::time::sleep(Duration::from_millis(200)).await;
    calls.responses(crate::ticker::now(), &alive, &mut m);
    let r = calls
        .take_resolved::<bank::Slow, ()>()
        .pop()
        .expect("swept");
    assert_eq!(r.outcome, Err(CallError::TimedOut));
    tokio::time::sleep(Duration::from_millis(500)).await;
    calls.responses(crate::ticker::now(), &alive, &mut m);
    let c = calls.counts();
    assert_eq!(
        (c.results_timed_out, c.results_ok, c.results_late),
        (1, 0, 1)
    );
}

#[tokio::test]
async fn the_stop_serves_what_was_queued_then_refuses_gone() {
    let (route, stop) = spawn(Bank::serve(Vault::default()), |s| s);
    let h = BankHandle::from_route(route);
    let queued = tokio::spawn({
        let h = h.clone();
        async move { h.slow(50).await }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    tokio::time::timeout(Duration::from_secs(5), stop.request_stop())
        .await
        .expect("the service ends on its stop")
        .expect("no panic");
    // Answered before the task ended.
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(5), queued)
            .await
            .expect("answered before the end")
            .expect("caller"),
        Ok(())
    );
    assert_eq!(h.charge(1).await, Err(CallError::Refused(Refusal::Gone)));
}

#[tokio::test]
async fn a_full_mailbox_refuses_the_handle_at_once() {
    let (route, _stop) = spawn(Bank::serve(Vault::default()), |s| s.mailbox(1));
    let h = BankHandle::from_route(route.clone());
    // A current-thread runtime: the host runs only when this test
    // yields, so the second call of this one poll finds the 1-slot
    // mailbox full.
    let (ra, rb) = tokio::join!(h.charge(1), h.charge(2));
    assert_eq!(ra, Ok(499));
    assert_eq!(rb, Err(CallError::Refused(Refusal::Full)));
}

#[test]
fn a_registered_kind_hands_out_both_clients_and_a_missing_one_none() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        let mut services = Services::new();
        assert!(services.try_client::<BankClient>().is_none());
        services
            .service(Bank::serve(Vault::default()), |s| s)
            .expect("registered once");
        assert!(services.try_client::<BankClient>().is_some());
        assert!(services.try_client::<BankHandle>().is_some());
        assert!(services.try_client::<ShopHandle>().is_none());
        assert_eq!(services.into_stops().len(), 1);
    });
}

/// B169: a second registration of one kind is an error, not a panic;
/// nothing is spawned for it and the first route stays.
#[test]
fn registering_a_kind_twice_is_an_error() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        let mut services = Services::new();
        services
            .service(Bank::serve(Vault::default()), |s| s)
            .expect("the first registration");
        let again = services.service(Bank::serve(Vault::default()), |s| s);
        assert_eq!(
            again.err(),
            Some(RegisterError::Duplicate { service: "Bank" })
        );
        assert_eq!(
            RegisterError::Duplicate { service: "Bank" }.to_string(),
            "service Bank is registered twice"
        );
        assert!(services.try_client::<BankClient>().is_some());
        assert_eq!(services.into_stops().len(), 1, "nothing spawned for it");
    });
}

/// An outcome the router builds is the call's own type.
#[test]
fn generated_call_types_carry_the_methods_types() {
    fn is<C: crate::calls::Call<Response = R, Error = E>, R, E>() {}
    is::<bank::Charge, u64, String>();
    let _: Outcome<bank::Charge> = Ok(1);
    assert_eq!(<bank::Kind as ServiceKind>::METHODS.len(), 5);
    assert_eq!(<bank::Kind as ServiceKind>::NAME, "Bank");
    let _ = CallKey { origin: 0, seq: 0 };
}
