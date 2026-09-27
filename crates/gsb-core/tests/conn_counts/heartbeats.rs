//! The heartbeat throttle's surplus (SECURITY §3.2, BACKLOG B56): the
//! actor answers at most one heartbeat per second and counts the rest,
//! per phase — and until B56 those counts reached only a debug line.
//! Now they ride the samples, as deltas.

use super::*;
use rig::Conn;

async fn heartbeats(c: &Conn, n: u64) {
    for tick in 0..n {
        c.send(op::base::HEARTBEAT, Heartbeat { tick }.encode_to_vec())
            .await;
    }
}

/// Four heartbeats before authentication and three after, each burst
/// well inside one second: the first of each burst is answered (auth
/// success resets the throttle), the rest are counted in their own
/// phase's counter. The pauses let the actor flush between the bursts
/// (its flush interval is 500 ms): each surplus is carried once, however
/// many samples the session sends.
#[tokio::test]
async fn the_unanswered_heartbeats_reach_the_samples_by_phase() {
    let mut c = Conn::open(64);
    heartbeats(&c, 4).await;
    c.until(op::base::HEARTBEAT_ACK).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    c.auth().await; // flushes the pre-auth burst
    heartbeats(&c, 3).await;
    c.until(op::base::HEARTBEAT_ACK).await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    heartbeats(&c, 1).await; // answered; flushes the authed burst
    c.until(op::base::HEARTBEAT_ACK).await;
    let sum = c.close().await;
    assert_eq!(sum.heartbeats_throttled_preauth, 3);
    assert_eq!(sum.heartbeats_throttled_authed, 2);
    assert_eq!(sum.violations, 0, "not a violation");
}
