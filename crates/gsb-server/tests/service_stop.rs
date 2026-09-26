//! A game service's explicit stop (BACKLOG F5, DESIGN §9.2), end to end.
//!
//! The game here is a test module whose rooms SETTLE with a ledger
//! service on their way out: each room's `on_shutdown` blocks for a while
//! (a long final settlement), then sends the ledger one message. The
//! ledger is registered through `RegistryParts::service`, so
//! `ServerHandle::stop` must (1) wait until every room has run that hook,
//! (2) only then ask the ledger to stop — its stop message queues BEHIND
//! the settlements, so all of them are served — and (3) return only after
//! the ledger ended. A service that ignores its stop request is aborted at
//! the grace and reported; `stop` still completes.

use std::time::{Duration, Instant};

use gsb_core::id::RoomId;
use gsb_core::registry::RoomStatus;
use tokio::sync::mpsc;

#[path = "service_stop/ledger.rs"]
mod ledger;
use ledger::{Event, LedgerGame};

const ROOMS: u64 = 2;

/// Start the ledger game with [`ROOMS`] rooms, wait until they all run,
/// stop the server and return the report, how long `stop` took, and every
/// event recorded by the time it returned (in order).
async fn run(deaf: bool) -> (gsb_server::StopReport, Duration, Vec<Event>) {
    let (events_tx, mut events) = mpsc::channel::<Event>(64);
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: ROOMS,
        ..Default::default()
    };
    let game = LedgerGame::new(events_tx, deaf);
    let handle = gsb_server::start_game_server(Box::new(game), cfg)
        .await
        .expect("the ledger game starts");
    for id in 1..=ROOMS {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(
            handle.room_status(RoomId(id)).await,
            Ok(RoomStatus::Running { .. })
        ) {
            assert!(Instant::now() < deadline, "room {id} never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let started = Instant::now();
    let report = tokio::time::timeout(Duration::from_secs(10), handle.stop())
        .await
        .expect("stop() completes");
    let took = started.elapsed();
    let mut seen = Vec::new();
    while let Ok(e) = events.try_recv() {
        seen.push(e);
    }
    (report, took, seen)
}

/// Every room's teardown, then every settlement it sent, then the
/// ledger's stop — all recorded before `stop` returned.
fn assert_settled_then_stopped(seen: &[Event]) {
    assert_eq!(
        seen.last(),
        Some(&Event::Stopped),
        "the ledger had not stopped when stop() returned: {seen:?}"
    );
    for id in 1..=ROOMS {
        let down = seen.iter().position(|e| *e == Event::RoomDown(id));
        let settled = seen.iter().position(|e| *e == Event::Settled(id));
        assert!(
            matches!((down, settled), (Some(d), Some(s)) if d < s),
            "room {id}: teardown then settlement expected: {seen:?}"
        );
    }
    assert_eq!(seen.len() as u64, 2 * ROOMS + 1, "{seen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_service_serves_the_rooms_last_words_then_stops_before_stop_returns() {
    let (report, took, seen) = run(false).await;
    assert_settled_then_stopped(&seen);
    assert!(report.rooms_finished, "{report:?}");
    assert_eq!((report.services_ended, report.services_aborted), (1, 0));
    assert!(took < Duration::from_secs(2), "stop took {took:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_that_never_ends_is_aborted_and_stop_still_completes() {
    let (report, took, seen) = run(true).await;
    assert_settled_then_stopped(&seen);
    assert!(report.rooms_finished, "{report:?}");
    assert_eq!((report.services_ended, report.services_aborted), (1, 1));
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(3),
        "the deaf service got the grace, and no more: {took:?}"
    );
}
