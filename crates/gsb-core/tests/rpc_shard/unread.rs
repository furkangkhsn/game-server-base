//! The shard actor's side of BACKLOG B36 (the room's
//! `tests/rpc/unread.rs`): a session that ends with requests still
//! unread in its action channel — its leave landing before the READ
//! pull, or a rejoin superseding it — counts them in
//! `requests_dropped_unread`, so every request that reached the shard
//! ends in exactly one bucket.

use super::*;

/// A barrier request from `conn`, answered on the next tick; returns
/// the sample of that step.
async fn sample_after_barrier(h: &mut Harness, conn: ConnectionId, id: u64) -> RoomSample {
    h.request(0, conn, id, OP_LOCAL).await;
    h.tick();
    let replies = h.private_replies(0, conn, Duration::from_secs(2)).await;
    assert_eq!(replies, vec![(id, true)], "the barrier's own answer");
    h.latest_sample(0)
}

/// The leave lands in the same tick window as three requests: the
/// shard's CONTROL phase ends the session before READ, and the three are
/// counted as unread — once, beside the one answered before.
#[tokio::test]
async fn shard_requests_unread_when_the_leave_lands_are_counted() {
    let mut h = Harness::new(1, bucket_cfg(1)).await;
    h.join(0, ConnectionId(1)).await;
    h.join(0, ConnectionId(2)).await;

    h.request(0, ConnectionId(1), 1, OP_LOCAL).await;
    h.tick();
    let replies = h
        .private_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(1, true)]);

    h.request(0, ConnectionId(1), 2, OP_LOCAL).await;
    h.request(0, ConnectionId(1), 3, OP_EXT).await;
    h.request(0, ConnectionId(1), 4, OP_REJECT).await;
    h.leave(0, ConnectionId(1), 1).await;

    let s = sample_after_barrier(&mut h, ConnectionId(2), 9).await;
    assert_eq!(s.leaves, 1, "the leave went through");
    assert_eq!(s.requests_local, 2, "the answered one and the barrier");
    assert_eq!(s.requests_external, 0, "the unread external never ran");
    assert_eq!(s.requests_rejected_logic, 0, "the unread reject never ran");
    assert_eq!(
        s.requests_dropped_unread, 3,
        "the three unread at the leave"
    );
    h.shutdown().await;
}

/// A rejoin on the same connection supersedes its stale row: the old
/// session's unread requests are counted, the new session starts clean.
#[tokio::test]
async fn shard_superseding_rejoin_counts_the_old_sessions_unread_requests() {
    let mut h = Harness::new(1, bucket_cfg(1)).await;
    h.join(0, ConnectionId(1)).await;
    h.request(0, ConnectionId(1), 1, OP_LOCAL).await;
    h.request(0, ConnectionId(1), 2, OP_LOCAL).await;
    h.join(0, ConnectionId(1)).await;

    let s = sample_after_barrier(&mut h, ConnectionId(1), 3).await;
    assert_eq!(s.requests_dropped_unread, 2, "the old session's two");
    assert_eq!(s.requests_local, 1, "only the new session's barrier ran");
    h.shutdown().await;
}
