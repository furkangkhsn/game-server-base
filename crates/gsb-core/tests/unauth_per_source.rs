//! The registry's per-source cap on unauthenticated connections
//! (BACKLOG D12, SECURITY §4.3.1): a source over it is refused at birth
//! with its own counted reason, other sources are not, the source rule is
//! the doors' (D11), and a place comes back when a session authenticates
//! or closes — not when its AUTH fails (it is still unauthenticated).
//!
//! Synchronization: the registry handles its mailbox in order, so a
//! `RoomStatus` round trip after the opens is the barrier — once its
//! reply is in hand, each open's verdict (or its absence) is in the
//! connection's inbox.

use std::net::IpAddr;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use gsb_core::channel::{Mailbox, channel};
use gsb_core::conn::{ConnIn, ServerClose};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::registry::{Registry, RegistryMsg, RoomFactory};
use gsb_core::source::Source;
use gsb_core::ticker::Ticker;

/// A running registry: no total cap, the pool's cap `pool`, the
/// per-source cap `per_source`.
fn start(pool: Option<u64>, per_source: Option<u64>) -> (Mailbox<RegistryMsg>, JoinHandle<()>) {
    let (tx, rx) = channel::<RegistryMsg>(256);
    let (ticker, _ticker) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics, _metrics) = mpsc::channel(4096);
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room is created here"));
    let task = tokio::spawn(
        Registry::new(rx, tx.clone(), factory, ticker, metrics, None, pool, None)
            .with_unauth_per_source(per_source)
            .run(),
    );
    (tx, task)
}

/// Open connection `n` from `from` (`None`: no peer address).
async fn open(tx: &Mailbox<RegistryMsg>, n: u64, from: Option<&str>) -> mpsc::Receiver<ConnIn> {
    let (inbox, rx) = channel::<ConnIn>(8);
    let source = from.map(|s| Source::of(s.parse::<IpAddr>().expect("an address")));
    tx.send(RegistryMsg::ConnOpened {
        conn: ConnectionId(n),
        inbox,
        source,
    })
    .await
    .expect("registry alive");
    rx
}

/// Everything sent so far has been handled.
async fn barrier(tx: &Mailbox<RegistryMsg>) {
    let (reply, rx) = oneshot::channel();
    tx.send(RegistryMsg::RoomStatus {
        id: RoomId(999),
        reply,
    })
    .await
    .expect("registry alive");
    rx.await.expect("a status");
}

/// The birth verdict the registry left in `inbox` (`None`: recorded).
async fn verdict(
    tx: &Mailbox<RegistryMsg>,
    inbox: &mut mpsc::Receiver<ConnIn>,
) -> Option<ServerClose> {
    barrier(tx).await;
    match inbox.try_recv() {
        Ok(ConnIn::ServerClosed { cause, reason }) => {
            if cause == ServerClose::UnauthSourceCap {
                assert!(reason.contains("per-source"), "reason: {reason}");
            }
            Some(cause)
        }
        Ok(other) => panic!("unexpected {other:?}"),
        Err(_) => None,
    }
}

async fn stop(tx: Mailbox<RegistryMsg>, task: JoinHandle<()>) {
    let _ = tx.send(RegistryMsg::Shutdown).await;
    drop(tx);
    let _ = task.await;
}

const OVER: Option<ServerClose> = Some(ServerClose::UnauthSourceCap);

#[tokio::test]
async fn over_the_cap_its_source_is_refused_others_are_not() {
    let (tx, task) = start(None, Some(2));
    let mut held = Vec::new();
    let mut n = 0;
    let mut next = || {
        n += 1;
        n
    };
    for from in ["10.0.0.1", "10.0.0.1"] {
        let mut a = open(&tx, next(), Some(from)).await;
        assert_eq!(verdict(&tx, &mut a).await, None, "{from} under the cap");
        held.push(a);
    }
    let mut a3 = open(&tx, next(), Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a3).await, OVER, "the third");
    let mut mapped = open(&tx, next(), Some("::ffff:10.0.0.1")).await;
    assert_eq!(
        verdict(&tx, &mut mapped).await,
        OVER,
        "mapped: the same source"
    );
    let mut b = open(&tx, next(), Some("10.0.0.2")).await;
    assert_eq!(verdict(&tx, &mut b).await, None, "another address");
    for from in ["2001:db8::1", "2001:db8::2"] {
        let mut c = open(&tx, next(), Some(from)).await;
        assert_eq!(verdict(&tx, &mut c).await, None, "{from}");
        held.push(c);
    }
    let mut c3 = open(&tx, next(), Some("2001:db8::ffff")).await;
    assert_eq!(verdict(&tx, &mut c3).await, OVER, "the same /64");
    let mut d = open(&tx, next(), Some("2001:db8:0:1::1")).await;
    assert_eq!(verdict(&tx, &mut d).await, None, "the next /64");
    // No peer address: never counted per source.
    for _ in 0..4 {
        let mut x = open(&tx, next(), None).await;
        assert_eq!(verdict(&tx, &mut x).await, None, "no address");
        held.push(x);
    }
    stop(tx, task).await;
}

#[tokio::test]
async fn a_place_comes_back_on_auth_and_on_close_not_on_a_failed_auth() {
    let (tx, task) = start(None, Some(1));
    let mut a1 = open(&tx, 1, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a1).await, None);
    let mut a2 = open(&tx, 2, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a2).await, OVER, "a1 holds the one place");
    // AUTH success: a1 leaves the count.
    tx.send(RegistryMsg::Authed {
        conn: ConnectionId(1),
    })
    .await
    .expect("registry alive");
    let mut a3 = open(&tx, 3, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a3).await, None, "a1 authenticated");
    // A failed AUTH sends nothing: a3 is still unauthenticated and keeps
    // its place.
    let mut a4 = open(&tx, 4, Some("10.0.0.1")).await;
    assert_eq!(
        verdict(&tx, &mut a4).await,
        OVER,
        "a3 still unauthenticated"
    );
    // Its close gives the place back.
    tx.send(RegistryMsg::ConnClosed {
        conn: ConnectionId(3),
        verdict: None,
    })
    .await
    .expect("registry alive");
    let mut a5 = open(&tx, 5, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a5).await, None, "a3 closed");
    // The authenticated a1 closing changes nothing for the source.
    tx.send(RegistryMsg::ConnClosed {
        conn: ConnectionId(1),
        verdict: None,
    })
    .await
    .expect("registry alive");
    let mut a6 = open(&tx, 6, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a6).await, OVER, "a5 holds it");
    stop(tx, task).await;
}

/// The default (and `0`) is today's registry: one source may hold the
/// whole pool.
#[tokio::test]
async fn unset_or_zero_is_no_cap() {
    for cap in [None, Some(0)] {
        let (tx, task) = start(None, cap);
        let mut held = Vec::new();
        for n in 1..=16 {
            let mut a = open(&tx, n, Some("127.0.0.1")).await;
            assert_eq!(verdict(&tx, &mut a).await, None, "{cap:?}: conn {n}");
            held.push(a);
        }
        stop(tx, task).await;
    }
}

/// A source over its cap is refused under its own reason even when the
/// pool is full too; another source at the full pool gets the pool's.
#[tokio::test]
async fn the_source_s_cap_and_the_pool_s_are_told_apart() {
    let (tx, task) = start(Some(3), Some(2));
    let mut held = Vec::new();
    for (n, from) in [(1, "10.0.0.1"), (2, "10.0.0.1"), (3, "10.0.0.2")] {
        let mut a = open(&tx, n, Some(from)).await;
        assert_eq!(verdict(&tx, &mut a).await, None, "conn {n}");
        held.push(a);
    }
    let mut a = open(&tx, 4, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a).await, OVER, "its source's cap");
    let mut b = open(&tx, 5, Some("10.0.0.2")).await;
    assert_eq!(
        verdict(&tx, &mut b).await,
        Some(ServerClose::UnauthCap),
        "the pool's"
    );
    stop(tx, task).await;
}
