//! The registry's control-plane losses (BACKLOG B57): a join or a close
//! op its bounded per-connection op queue refused. The join's client was
//! answered `ERROR` "registry unavailable" and the close's detach was
//! lost, and neither was counted anywhere.
//!
//! Filling the queue is deterministic here: the test's current-thread
//! runtime runs the registry through its whole backlog before the
//! connection's dispatcher runs even once, so the dispatcher's 16-deep
//! queue takes the first 16 joins and refuses the rest.

use super::*;

/// Forty joins of one connection queued back to back, then its close:
/// the dispatcher's queue refuses 24 joins — each a dropped reply, each
/// counted — and the close behind them.
#[tokio::test]
async fn joins_and_a_close_the_op_queue_refused_are_counted() {
    let (tx, mut metrics, handle) = start_observed();
    create_room(&tx, RoomId(1)).await;
    let conn = ConnectionId(1);
    open_conn(&tx, conn).await;

    let mut replies = Vec::new();
    let mut outs = Vec::new();
    for _ in 0..40 {
        let (out, out_rx) = mpsc::channel::<FrameBatch>(64);
        outs.push(out_rx);
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(RegistryMsg::SpawnPlayer {
            conn,
            room: RoomId(1),
            out,
            identity: String::new(),
            reply,
        })
        .await
        .expect("registry gone");
        replies.push(reply_rx);
    }
    close_conn(&tx, conn).await;
    settle(&tx).await;
    let s = latest(&mut metrics);

    let mut refused = 0u64;
    for r in replies {
        let answer = tokio::time::timeout(WAIT, r)
            .await
            .expect("an answer in time");
        if answer.is_err() {
            refused += 1; // the op, and its reply, were dropped
        }
    }
    assert_eq!(refused, 24, "16 queued; the rest refused");
    assert_eq!(s.join_ops_dropped, refused, "each one counted");
    assert_eq!(s.close_ops_dropped, 1, "the close behind them");

    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not stop")
        .expect("registry task panicked");
}
