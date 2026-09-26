//! The two building blocks on their own: the drop barrier releases on the
//! LAST token only, and a service's stop request runs exactly when asked.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;

const SHORT: Duration = Duration::from_millis(50);

#[tokio::test]
async fn the_barrier_waits_for_every_hold() {
    let (hold, released) = hold();
    let other = hold.clone();
    let mut waiter = tokio::spawn(released.wait());
    drop(hold);
    assert!(
        tokio::time::timeout(SHORT, &mut waiter).await.is_err(),
        "released while a clone was still held"
    );
    drop(other);
    tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the last drop must release the waiter")
        .expect("waiter panicked");
}

#[tokio::test]
async fn a_barrier_with_no_holders_left_releases_at_once() {
    let (hold, released) = hold();
    drop(hold);
    tokio::time::timeout(SHORT, released.wait())
        .await
        .expect("nothing holds it");
}

#[tokio::test]
async fn request_stop_asks_then_hands_back_the_task() {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = rx.await;
    });
    let service = Service::new("probe", task, move || {
        let _ = tx.send(());
    });
    assert_eq!(service.name(), "probe");
    tokio::time::timeout(Duration::from_secs(5), service.request_stop())
        .await
        .expect("the stop request must reach the task")
        .expect("task panicked");
}

#[tokio::test]
async fn dropping_a_service_neither_asks_nor_stops_it() {
    let asked = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&asked);
    let (keep, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = rx.await;
    });
    let probe = task.abort_handle();
    drop(Service::new("probe", task, move || {
        flag.store(true, Ordering::SeqCst)
    }));
    tokio::task::yield_now().await;
    assert!(
        !asked.load(Ordering::SeqCst),
        "a drop is not a stop request"
    );
    assert!(!probe.is_finished(), "the task keeps its own life");
    drop(keep);
}
