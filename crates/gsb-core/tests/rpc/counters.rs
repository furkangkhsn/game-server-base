//! The room's RPC counters on the room's own path.
//!
//! `requests_local`, `requests_external`, `pending_requests` and
//! `requests_timed_out` are the sizing inputs for the pending caps
//! (`docs/RPC-CONTROL-PLANE.md` §6.1): how much of the load is answered
//! in-tick, how much is delegated, how deep the in-flight queue actually
//! runs, and how often the client-visible timeout fires. Each of those
//! behaviours has been tested since the feature landed — through the
//! REPLY, which is what the client sees — but the counters were read
//! only on the shard actor's copy of the path, or asserted at zero. A
//! room-side counter wired to the wrong branch (or to nothing) would
//! pass every one of those reply tests.
//!
//! `leaves` is here for the same reason and had no test at all, on
//! either actor: the control plane's leave funnel is exercised
//! constantly, its counter never read.
//!
//! Synchronization follows `Harness::latest_room_sample`'s contract: the
//! sample is taken immediately after a barrier that proves the step
//! finished, with no newer tick in between. Two barriers are used — a
//! private reply read (the step's broadcast phase ran) and
//! `next_resolver` (the delegated worker was spawned and polled, which
//! only happens once the room yields at its next `tick_rx.recv()`).

use super::*;

/// A room that samples on every step (like [`bucket_cfg`]) and times
/// requests out fast enough for a test to sit through the deadline.
fn timeout_cfg(timeout: Duration) -> RoomConfig {
    RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        metrics_cadence_hz: 30.0,
        request_timeout: timeout,
        ..Default::default()
    }
}

/// `requests_local` counts each request the logic answered in the SAME
/// tick, one per request and not one per tick.
///
/// Two requests arriving in one tick are the case that separates the
/// two: a per-tick counter reads 1 here and would keep reading "one
/// room-local request per tick" under any load, which is precisely the
/// number the cap sizing divides by.
#[tokio::test]
async fn requests_local_counts_every_same_tick_answer_not_every_tick() {
    let mut h = Harness::new(bucket_cfg()).await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 1, OP_LOCAL, &[]).await;
    h.request(ConnectionId(1), 2, OP_LOCAL, &[]).await;
    h.tick();

    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(1, true), (2, true)], "both answered");

    let s = h.latest_room_sample();
    assert_eq!(
        s.requests_local, 2,
        "two room-local answers in one tick must count twice"
    );
    assert_eq!(
        s.requests_external, 0,
        "a room-local answer is not a delegation"
    );
    assert_eq!(
        s.pending_requests, 0,
        "nothing was registered pending: the answers rode this tick"
    );
    h.shutdown().await;
}

/// `requests_external` counts the delegation and `pending_requests` is
/// the in-flight GAUGE that rises with it and falls when the answer
/// lands.
///
/// The gauge is the half that cannot be inferred from the counters: a
/// cumulative `requests_external` that never came back down would look
/// identical in a log line, and `pending_requests` is exactly what an
/// operator compares against `max_pending_requests`.
#[tokio::test]
async fn requests_external_counts_the_delegation_and_pending_tracks_flight() {
    let mut h = Harness::new(bucket_cfg()).await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 41, OP_EXT, &[]).await;
    h.tick();
    // Barrier: the resolver is handed over from INSIDE the future, on
    // its first poll — which can only happen after the room finished
    // the step that registered it (and its sample send with it).
    let resolver = h.next_resolver().await;
    assert_eq!(resolver.id, 41);

    let s = h.latest_room_sample();
    assert_eq!(s.requests_external, 1, "one delegated request");
    assert_eq!(
        s.requests_local, 0,
        "a delegated request is not a room-local answer"
    );
    assert_eq!(s.pending_requests, 1, "one request in flight right now");

    // The answer lands: the gauge must come back down while the
    // cumulative counter stays.
    resolver
        .resolve
        .send(Ok(b"ok".to_vec()))
        .expect("resolver alive");
    let replies = h
        .wait_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(41, true)]);

    let s = h.latest_room_sample();
    assert_eq!(
        s.pending_requests, 0,
        "the answered request left the in-flight set"
    );
    assert_eq!(
        s.requests_external, 1,
        "the cumulative delegation count does not fall back"
    );
    assert_eq!(
        s.requests_timed_out, 0,
        "an answered request is not a timeout"
    );
    h.shutdown().await;
}

/// `requests_timed_out` counts the sweep that answers a stuck request.
///
/// It had no positive test anywhere: the timeout BEHAVIOUR is locked
/// (`timeout_swept_exactly_one_answer`) through the reply the client
/// gets, and every counter assertion on this field asserted it at ZERO.
/// A field that is only ever asserted to be zero is indistinguishable
/// from a field that is never written — which is exactly the defect two
/// earlier rounds found in this metric surface.
#[tokio::test]
async fn requests_timed_out_counts_the_sweep() {
    let mut h = Harness::new(timeout_cfg(Duration::from_millis(80))).await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 51, OP_EXT, &[]).await;
    h.tick();
    // Held, never resolved: the room's deadline is the only thing that
    // can answer it.
    let _resolver = h.next_resolver().await;

    let s = h.latest_room_sample();
    assert_eq!(s.pending_requests, 1, "in flight, before the deadline");
    assert_eq!(s.requests_timed_out, 0, "the sweep has not fired yet");

    // Past the deadline, then a tick: the sweep answers it.
    tokio::time::sleep(Duration::from_millis(130)).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(51, false)], "the timeout reply");

    let s = h.latest_room_sample();
    assert_eq!(
        s.requests_timed_out, 1,
        "the swept request must be counted in the timeout bucket"
    );
    assert_eq!(
        s.pending_requests, 0,
        "the swept request left the in-flight set"
    );
    assert_eq!(
        s.requests_late, 0,
        "the worker was dropped without reporting: there is no late report"
    );
    assert_eq!(
        s.requests_local, 0,
        "a timeout sweep is not a room-local answer"
    );
    h.shutdown().await;
}

/// `leaves` counts the control plane's leave funnel, independently of
/// `joins` and of the `members` gauge.
///
/// The three are read together in a report line, and the failure this
/// pins is the cheap one: a `leaves` wired to the same place as `joins`
/// (or never written) reads 0 or 2 here while `members` still falls to
/// 1, so the gauge alone cannot stand in for it.
#[tokio::test]
async fn leaves_counts_the_control_plane_leave() {
    let mut h = Harness::new(bucket_cfg()).await;
    h.join(ConnectionId(1)).await;
    h.join(ConnectionId(2)).await;

    h.leave(ConnectionId(1), 1).await;

    // Barrier: a request from the REMAINING connection, whose reply
    // proves a later step (and its sample) completed. The counters are
    // cumulative, so the leave is in it.
    h.request(ConnectionId(2), 9, OP_LOCAL, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(2), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(9, true)]);

    let s = h.latest_room_sample();
    assert_eq!(s.joins, 2, "both connections joined");
    assert_eq!(s.leaves, 1, "exactly one of them left");
    assert_eq!(s.members, 1, "and the gauge agrees");
    h.shutdown().await;
}
