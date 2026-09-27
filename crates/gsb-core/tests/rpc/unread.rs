//! Requests a session sent that the room never read (BACKLOG B36).
//!
//! The room's CONTROL phase runs before its READ phase: a leave that
//! lands in the same tick window as the requests sent just before it
//! removes the row — and drops its action channel — before the READ
//! pull ever sees them. Every other request ends in exactly one bucket
//! (answered, one of the rejections, refused, or accepted pending); these
//! ended in none, which is the gap the loadgen's RPC ledger showed
//! (`sent − req_ext − req_refused` > 0 on a run whose clients all left).
//!
//! The ledger these tests hold the room to: every request that reached a
//! session's action channel is counted exactly once, so the sum of the
//! buckets equals what was sent.

use super::*;

/// Every bucket a request that reached the room can end in, summed.
fn accounted(s: &RoomSample) -> u64 {
    s.requests_local
        + s.requests_external
        + s.requests_rejected_malformed
        + s.requests_rejected_dup
        + s.requests_rejected_no_handler
        + s.requests_rejected_logic
        + s.requests_rejected_conn_cap
        + s.requests_rejected_room_cap
        + s.requests_refused_congested
        + s.requests_dropped_unread
}

/// A plain (non-request) game-band action.
async fn plain_action(h: &mut Harness, conn: ConnectionId) {
    let a = Action {
        conn,
        player: PlayerId(conn.0),
        op: 0x2001,
        payload: bytes::Bytes::new(),
    };
    h.actions
        .get(&conn)
        .expect("conn joined")
        .send(a)
        .await
        .expect("action mailbox alive");
}

/// A barrier: a request from `conn` answered on the next tick proves
/// that step (and its sample) finished; returns that sample.
async fn sample_after_barrier(h: &mut Harness, conn: ConnectionId, id: u64) -> RoomSample {
    h.request(conn, id, OP_LOCAL, &[]).await;
    h.tick();
    let replies = h.private_replies(conn, Duration::from_secs(2)).await;
    assert_eq!(replies, vec![(id, true)], "the barrier's own answer");
    h.latest_room_sample()
}

/// Requests still unread in the channel when the leave lands are
/// counted — once each, and only the requests (a plain action beside
/// them is not a request).
#[tokio::test]
async fn requests_unread_when_the_leave_lands_are_counted() {
    let mut h = Harness::new(bucket_cfg()).await;
    h.join(ConnectionId(1)).await;
    h.join(ConnectionId(2)).await;

    // One request read and answered before the leave: it is in its own
    // bucket and must not be counted again.
    h.request(ConnectionId(1), 1, OP_LOCAL, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(1, true)]);

    // Three more, a plain action, and the leave — all in one tick window
    // (the client's last burst right before its LEAVE_ROOM_REQ).
    h.request(ConnectionId(1), 2, OP_LOCAL, &[]).await;
    h.request(ConnectionId(1), 3, OP_EXT, &[]).await;
    plain_action(&mut h, ConnectionId(1)).await;
    h.request(ConnectionId(1), 4, OP_REJECT, &[]).await;
    h.leave(ConnectionId(1), 1).await;

    // Conn 2's barrier request is counted too (room-local): 5 sent.
    let s = sample_after_barrier(&mut h, ConnectionId(2), 9).await;
    assert_eq!(s.leaves, 1, "the leave went through");
    assert_eq!(
        s.requests_local, 2,
        "the answered one and the barrier: the unread ones were not processed"
    );
    assert_eq!(s.requests_external, 0, "the unread external never ran");
    assert_eq!(
        s.requests_dropped_unread, 3,
        "the three requests unread at the leave — not the plain action, not \
         the one answered before it"
    );
    assert_eq!(accounted(&s), 5, "sent = every bucket");
    h.shutdown().await;
}
