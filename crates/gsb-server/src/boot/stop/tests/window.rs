//! The stop's order (BACKLOG F41): no connection is handed to the
//! registry after its `Shutdown`, so no connection actor outlives `stop`.
//!
//! The registry stops reading at its `Shutdown`: a `ConnOpened` queued
//! behind it is lost with its mailbox, and one sent after it fails. The
//! accept loop spawns the connection actor either way, and that actor
//! never gets its `ConnIn::Shutdown` — it lives until its peer closes the
//! socket or the idle window (30 s) ends it, past the stop, holding the
//! collector's session channel open, so the final report goes out at its
//! grace without the actor's last word (`final_report_complete = false`).
//! Before the fix `stop` sent the `Shutdown` first and closed the doors
//! after it; a peer an accept loop took in between was such an actor.
//!
//! The window is made deterministic with a busy registry: its mailbox
//! (capacity one) is full, so the stop's send parks, and the peer's
//! `ConnOpened` parks behind it — the mailbox's senders are served in
//! order. Everything else is the real thing: the accept loop, the
//! connection actor, the collector and `stop` itself, on the paused clock
//! (a settle is "every task has run until it waits on something").

use std::time::Duration;

use gsb_core::id::ConnectionId;

mod rig;
use rig::{BUSY, Seen, read, server, settle};

/// THE WINDOW: a peer that arrives once the stop is under way, while
/// the registry is busy. It is either registered ahead of the `Shutdown`
/// or refused at the closed door — never handed over behind it.
#[tokio::test(start_paused = true)]
async fn a_peer_arriving_during_the_stop_is_not_handed_over_behind_the_shutdown() {
    let (handle, door, go, seen) = server();
    let stop = tokio::spawn(handle.stop());
    settle().await;
    door.peers.add_permits(1);
    settle().await;
    go.send(()).expect("the registry waits");
    let report = stop.await.expect("stop completes");
    let log = read(seen);
    let shutdown = log.iter().position(|s| *s == Seen::Shutdown);
    let shutdown = shutdown.expect("the registry got its Shutdown");
    assert!(
        !log[shutdown..].iter().any(|s| matches!(s, Seen::Opened(_))),
        "a connection was handed over behind the Shutdown: {log:?}"
    );
    assert_eq!(
        door.peers.available_permits(),
        1,
        "the peer was refused at the closed door"
    );
    assert_eq!(report.accept_loops_ended, 1);
    assert!(
        report.final_report_complete,
        "a connection actor outlived the stop"
    );
}

/// A peer taken before the stop, whose `ConnOpened` still waits on the
/// busy registry, is registered ahead of the `Shutdown`, told, and ends:
/// the stop waits for the accept loop to hand it over.
#[tokio::test(start_paused = true)]
async fn a_peer_taken_before_the_stop_is_registered_and_told() {
    let (handle, door, go, seen) = server();
    door.peers.add_permits(1);
    settle().await;
    let stop = tokio::spawn(handle.stop());
    settle().await;
    go.send(()).expect("the registry waits");
    let report = stop.await.expect("stop completes");
    let log = read(seen);
    let first = ConnectionId(1);
    assert_eq!(
        log[..3],
        [Seen::Closed(BUSY), Seen::Opened(first), Seen::Shutdown],
        "{log:?}"
    );
    assert!(
        log.contains(&Seen::Closed(first)),
        "the actor ended: {log:?}"
    );
    assert_eq!(report.accept_loops_ended, 1);
    assert!(report.final_report_complete);
}

/// THE SAME POLL: a peer the door lets through AS it closes — ready when
/// the stop closes the door, before the woken accept loop has run (a
/// door's pending accept is biased to the connection it already has,
/// `Door::admit`). The loop hands it over after the close, so closing
/// the doors before the `Shutdown` is not enough: the stop must wait for
/// the loops to have handed over what they took.
#[tokio::test(start_paused = true)]
async fn a_peer_let_through_as_the_door_closes_is_registered_ahead_of_the_shutdown() {
    let (handle, door, go, seen) = server();
    settle().await;
    door.peers.add_permits(1);
    let mut stop = Box::pin(handle.stop());
    // The stop's first poll runs before the woken accept loop does.
    let _ = tokio::time::timeout(Duration::ZERO, stop.as_mut()).await;
    settle().await;
    go.send(()).expect("the registry waits");
    let report = stop.await;
    let log = read(seen);
    let first = ConnectionId(1);
    assert_eq!(
        log[..3],
        [Seen::Closed(BUSY), Seen::Opened(first), Seen::Shutdown],
        "{log:?}"
    );
    assert!(
        log.contains(&Seen::Closed(first)),
        "the actor ended: {log:?}"
    );
    assert!(
        report.final_report_complete,
        "a connection actor outlived the stop"
    );
}
