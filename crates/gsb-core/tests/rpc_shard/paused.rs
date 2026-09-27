//! The shard's request timeout on the tick clock (BACKLOG F16; the
//! room's `rpc::paused`): under tokio's PAUSED clock the sweep fires when
//! the paused time passes the deadline.

use super::*;

#[tokio::test(start_paused = true)]
async fn the_shard_timeout_sweep_runs_on_the_paused_clock() {
    let real = Instant::now();
    let mut h = Harness::new(1, cfg(1)).await;
    let timeout = cfg(1).request_timeout;
    h.join(0, ConnectionId(1)).await;
    // Real time that the paused clock does not see: a deadline or a
    // sweep read off the wall clock would sit 300 ms apart from the
    // paused one, so a mixed-clock comparison cannot pass by accident.
    std::thread::sleep(Duration::from_millis(300));
    h.request(0, ConnectionId(1), 51, OP_EXT).await;
    h.tick(); // registered pending (at the latest): due one timeout on
    let _resolver = h.next_resolver(0).await; // never resolved

    tokio::time::sleep(timeout - Duration::from_millis(100)).await;
    h.tick();
    h.assert_no_private(0, ConnectionId(1), Duration::from_millis(40))
        .await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    h.tick();
    // One read, no ticking while waiting (`wait_replies` would tick on
    // until ANY clock's deadline passed).
    let replies = h
        .private_replies(0, ConnectionId(1), Duration::from_secs(2))
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
