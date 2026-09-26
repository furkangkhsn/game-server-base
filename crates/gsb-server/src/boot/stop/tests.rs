//! The backstop: loops that end are counted as ended, one that overruns
//! the grace is aborted — all under one deadline, so `stop` completes.

use std::time::{Duration, Instant};

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
        }
    );
    let took = started.elapsed();
    assert!(took >= grace, "the grace was given ({took:?})");
    assert!(
        took < grace * 2,
        "one deadline for all the loops, not one per loop ({took:?})"
    );
}
