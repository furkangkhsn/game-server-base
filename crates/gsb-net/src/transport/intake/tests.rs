//! The intake's rules, door-independent: handshakes run side by side,
//! the bound refuses and counts, a finished handshake holds its slot
//! until taken, the deadline and a failure are counted, and `close`
//! cuts every handshake in flight.

use std::future::pending;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures::FutureExt;
use tokio::sync::oneshot;

use super::*;
use crate::transport::is_listener_closed;

/// Far above anything here takes; far below any test timeout.
const PROMPT: Duration = Duration::from_secs(2);
/// A deadline no test handshake waits out.
const LONG: Duration = Duration::from_secs(60);

/// A stand-in endpoint, told apart by its peer's port.
fn endpoint(port: u16) -> Endpoint {
    Endpoint::new(|_, _, _, _| unreachable!("never started")).with_peer(peer(port))
}

fn peer(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A handshake that never finishes, and a receiver that errors once the
/// handshake's future is dropped (cut).
fn stuck() -> (
    impl Future<Output = io::Result<Endpoint>> + Send + 'static,
    oneshot::Receiver<()>,
) {
    let (alive, dropped) = oneshot::channel::<()>();
    let fut = async move {
        let _alive = alive;
        pending::<io::Result<Endpoint>>().await
    };
    (fut, dropped)
}

/// A door's failed handshake is counted — once, promptly — and nothing
/// reaches its accept loop (shared with the TLS and QUIC suites).
pub(crate) async fn a_failed_handshake_is_counted(listener: &Arc<dyn crate::transport::Listener>) {
    let stats = || listener.handshake_stats().expect("a handshaking door");
    until("the failure counted", || stats().failed == 1).await;
    let s = stats();
    assert_eq!((s.completed, s.in_flight), (0, 0), "{s:?}");
    let next = tokio::time::timeout(Duration::from_millis(100), listener.clone().accept()).await;
    assert!(next.is_err(), "nothing reaches the accept loop");
}

/// Wait until `cond` holds (bounded by [`PROMPT`]).
pub(crate) async fn until(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + PROMPT;
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_stuck_handshake_does_not_hold_a_finished_one() {
    let intake = Intake::new("test", 4);
    let (slow, _dropped) = stuck();
    intake.spawn(intake.try_slot().unwrap(), peer(1), LONG, slow);
    intake.spawn(intake.try_slot().unwrap(), peer(2), LONG, async {
        Ok(endpoint(2))
    });
    let got = tokio::time::timeout(PROMPT, Arc::clone(&intake).next())
        .await
        .expect("the finished handshake is not behind the stuck one")
        .expect("an endpoint");
    assert_eq!(got.peer(), Some(peer(2)));
    assert_eq!(
        intake.stats().in_flight,
        1,
        "the stuck one still holds its slot"
    );
}

#[tokio::test]
async fn the_bound_refuses_and_counts() {
    let intake = Intake::new("test", 2);
    let a = intake.try_slot().expect("slot 1");
    let _b = intake.try_slot().expect("slot 2");
    assert!(intake.try_slot().is_none(), "over the bound");
    assert!(intake.try_slot().is_none(), "still over");
    let s = intake.stats();
    assert_eq!((s.in_flight, s.refused), (2, 2));
    drop(a);
    assert!(intake.try_slot().is_some(), "a released slot is free again");
    assert_eq!(intake.stats().refused, 2);
}

#[tokio::test]
async fn a_finished_handshake_holds_its_slot_until_taken() {
    let intake = Intake::new("test", 1);
    intake.spawn(intake.try_slot().unwrap(), peer(3), LONG, async {
        Ok(endpoint(3))
    });
    until("completed", || intake.stats().completed == 1).await;
    assert_eq!(intake.stats().in_flight, 1);
    assert!(intake.try_slot().is_none(), "the queued endpoint's slot");
    let got = Arc::clone(&intake).next().await.expect("the endpoint");
    assert_eq!(got.peer(), Some(peer(3)));
    assert_eq!(intake.stats().in_flight, 0, "taken: the slot is back");
}

#[tokio::test]
async fn a_handshake_past_its_deadline_is_cut_and_counted() {
    let intake = Intake::new("test", 4);
    let (slow, dropped) = stuck();
    intake.spawn(
        intake.try_slot().unwrap(),
        peer(4),
        Duration::from_millis(50),
        slow,
    );
    tokio::time::timeout(PROMPT, dropped)
        .await
        .expect("the deadline cut the handshake")
        .expect_err("dropped, not finished");
    until("timed out", || intake.stats().timed_out == 1).await;
    let s = intake.stats();
    assert_eq!((s.in_flight, s.completed, s.failed), (0, 0, 0));
}

#[tokio::test]
async fn a_failed_handshake_is_counted_not_queued() {
    let intake = Intake::new("test", 4);
    intake.spawn(intake.try_slot().unwrap(), peer(5), LONG, async {
        Err(io::Error::other("bad request"))
    });
    until("failed", || intake.stats().failed == 1).await;
    assert_eq!(intake.stats().in_flight, 0);
    let next = tokio::time::timeout(Duration::from_millis(100), Arc::clone(&intake).next()).await;
    assert!(next.is_err(), "nothing reaches the accept loop");
}

#[tokio::test]
async fn close_cuts_every_handshake_and_ends_the_accept() {
    let intake = Intake::new("test", 4);
    let (slow_a, dropped_a) = stuck();
    let (slow_b, dropped_b) = stuck();
    intake.spawn(intake.try_slot().unwrap(), peer(6), LONG, slow_a);
    intake.spawn(intake.try_slot().unwrap(), peer(7), LONG, slow_b);
    let parked = tokio::spawn(Arc::clone(&intake).next().map(|r| r.map(|_| ())));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!parked.is_finished());
    intake.close();
    for dropped in [dropped_a, dropped_b] {
        tokio::time::timeout(PROMPT, dropped)
            .await
            .expect("the close cut the handshake")
            .expect_err("dropped, not finished");
    }
    let e = tokio::time::timeout(PROMPT, parked)
        .await
        .expect("the close ended the accept")
        .expect("no panic")
        .expect_err("the closed error");
    assert!(is_listener_closed(&e));
    until("slots released", || intake.stats().in_flight == 0).await;
    until("both cuts counted (B74)", || intake.stats().cut_closed == 2).await;
    let s = intake.stats();
    assert_eq!(
        (s.timed_out, s.unaccepted_closed),
        (0, 0),
        "cut, not timed out"
    );
}

#[tokio::test]
async fn close_drops_what_is_queued() {
    let intake = Intake::new("test", 4);
    intake.spawn(intake.try_slot().unwrap(), peer(8), LONG, async {
        Ok(endpoint(8))
    });
    until("completed", || intake.stats().completed == 1).await;
    intake.close();
    let s = intake.stats();
    assert_eq!(s.in_flight, 0, "the queued endpoint is gone");
    assert_eq!((s.unaccepted_closed, s.cut_closed), (1, 0), "counted (B74)");
    let e = Arc::clone(&intake)
        .next()
        .await
        .map(|_| ())
        .expect_err("closed");
    assert!(is_listener_closed(&e));
}

#[tokio::test]
async fn the_last_handle_closes_the_door() {
    let intake = Intake::new("test", 4);
    let (slow, dropped) = stuck();
    intake.spawn(intake.try_slot().unwrap(), peer(9), LONG, slow);
    drop(IntakeHandle(Arc::clone(&intake)));
    tokio::time::timeout(PROMPT, dropped)
        .await
        .expect("the handle's drop cut the handshake")
        .expect_err("dropped, not finished");
    assert!(intake.door().is_closed());
}
