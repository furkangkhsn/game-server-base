//! The request timeout runs on the tick clock (BACKLOG F16): under
//! tokio's PAUSED clock a pending request is swept when the paused time
//! passes its deadline — five virtual seconds in milliseconds of real
//! time. Before F16 the deadline was a `std::time::Instant`, which the
//! paused clock does not move: the sweep never fired.

use super::*;

#[tokio::test(start_paused = true)]
async fn the_timeout_sweep_runs_on_the_paused_clock() {
    let real = Instant::now();
    let mut h = Harness::new(cfg()).await;
    let timeout = cfg().request_timeout;
    h.join(ConnectionId(1)).await;
    // Real time that the paused clock does not see: a deadline or a
    // sweep read off the wall clock would sit 300 ms apart from the
    // paused one, so a mixed-clock comparison cannot pass by accident.
    std::thread::sleep(Duration::from_millis(300));
    h.request(ConnectionId(1), 51, OP_EXT, &[]).await;
    h.tick(); // registered pending (at the latest): due one timeout on
    let _resolver = h.next_resolver().await; // never resolved

    tokio::time::sleep(timeout - Duration::from_millis(100)).await;
    h.tick();
    h.assert_no_private(ConnectionId(1), Duration::from_millis(40))
        .await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(51, false)], "swept on the paused clock");
    // Virtual: less real time than the timeout itself (a wall-clock
    // bound tighter than that would time the machine — BACKLOG F25).
    assert!(
        real.elapsed() < timeout,
        "a {timeout:?} timeout took {:?} of real time",
        real.elapsed()
    );
    h.shutdown().await;
}
