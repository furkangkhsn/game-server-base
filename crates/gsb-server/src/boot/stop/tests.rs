//! The backstop: loops that end are counted as ended, one that overruns
//! the grace is aborted — all under one deadline, so `stop` completes.

use std::time::{Duration, Instant};

use gsb_core::service::Service;

use super::*;

#[tokio::test]
async fn a_loop_that_overruns_the_grace_is_aborted_under_one_deadline() {
    let grace = Duration::from_millis(200);
    let ended = tokio::spawn(async {});
    let stuck = tokio::spawn(std::future::pending::<()>());
    let stuck_too = tokio::spawn(std::future::pending::<()>());
    let started = Instant::now();
    let report = end_accepts(vec![stuck, ended, stuck_too], grace).await;
    assert_eq!(
        report,
        StopReport {
            accept_loops_ended: 1,
            accept_loops_aborted: 2,
            ..StopReport::default()
        }
    );
    let took = started.elapsed();
    assert!(took >= grace, "the grace was given ({took:?})");
    assert!(
        took < grace * 2,
        "one deadline for all the loops, not one per loop ({took:?})"
    );
}

/// A service that ends when asked (its stop message closes the loop).
fn obedient(name: &'static str) -> Service {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = rx.await;
    });
    Service::new(name, task, move || {
        let _ = tx.send(());
    })
}

/// A service that ignores its stop request and never ends.
fn deaf(name: &'static str) -> Service {
    Service::new(name, tokio::spawn(std::future::pending::<()>()), || {})
}

#[tokio::test]
async fn services_are_asked_only_once_the_rooms_released() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let (hold, released) = gsb_core::service::hold();
    // A "room" whose teardown takes a while: it marks itself done, THEN
    // releases its token (the death watcher's order).
    let rooms_done = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&rooms_done);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        done.store(true, Ordering::SeqCst);
        drop(hold);
    });
    let asked_after_rooms = Arc::new(AtomicBool::new(false));
    let (seen, asked) = (Arc::clone(&rooms_done), Arc::clone(&asked_after_rooms));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = rx.await;
    });
    let service = Service::new("ledger", task, move || {
        asked.store(seen.load(Ordering::SeqCst), Ordering::SeqCst);
        let _ = tx.send(());
    });
    let mut report = StopReport::default();
    end_services(vec![service], released, Duration::from_secs(5), &mut report).await;
    assert!(
        asked_after_rooms.load(Ordering::SeqCst),
        "the service was asked to stop before the rooms finished"
    );
    assert!(report.rooms_finished);
    assert_eq!((report.services_ended, report.services_aborted), (1, 0));
}

#[tokio::test]
async fn a_deaf_service_is_aborted_under_one_deadline() {
    let grace = Duration::from_millis(200);
    let (hold, released) = gsb_core::service::hold();
    drop(hold);
    let services = vec![deaf("a"), obedient("b"), deaf("c")];
    let started = Instant::now();
    let mut report = StopReport::default();
    end_services(services, released, grace, &mut report).await;
    let took = started.elapsed();
    assert!(report.rooms_finished);
    assert_eq!((report.services_ended, report.services_aborted), (1, 2));
    assert!(took >= grace, "the grace was given ({took:?})");
    assert!(
        took < grace * 2,
        "one deadline for all the services ({took:?})"
    );
}

#[tokio::test]
async fn rooms_that_overrun_the_grace_do_not_hold_the_services_back() {
    let grace = Duration::from_millis(200);
    let (hold, released) = gsb_core::service::hold();
    let started = Instant::now();
    let mut report = StopReport::default();
    end_services(vec![obedient("ledger")], released, grace, &mut report).await;
    let took = started.elapsed();
    drop(hold);
    assert!(!report.rooms_finished, "a room still held its token");
    assert_eq!((report.services_ended, report.services_aborted), (1, 0));
    assert!(took < grace * 2, "the rooms' wait is bounded ({took:?})");
}
