//! What a server-decided end leaves unprocessed (BACKLOG B60). When the
//! server ends a session — a verdict from the registry or a pump, a
//! destroyed room, the stop, the violation or pre-auth budget, a dead
//! outbound path — the actor stops reading its inbox, and whatever the
//! reader had already queued behind the verdict was never processed and
//! never counted (an RPC request among them escaped the ledger), nor was
//! the frame that crossed the pre-auth budget. Now each is counted by
//! kind: RPC requests (a ledger term), game-band frames, and the other
//! base-band (control) frames.
//!
//! Every test queues the frames BEHIND the end before the actor runs
//! (the test runtime is single-threaded: nothing runs until the test
//! awaits the actor's end), so each count is exact.

use super::*;
use gsb_core::conn::ServerClose;
use gsb_core::id::RoomId;
use rig::{Conn, GAME_OP, game_frame};

fn heartbeat() -> Vec<u8> {
    Heartbeat { tick: 1 }.encode_to_vec()
}

/// A request, a game action and a heartbeat, queued.
async fn queue_one_of_each(c: &Conn) {
    c.send(op::base::RPC_REQ, vec![1]).await;
    c.send(GAME_OP, game_frame()).await;
    c.send(op::base::HEARTBEAT, heartbeat()).await;
}

fn assert_one_of_each(sum: &ConnSample, end: &str) {
    assert_eq!(sum.requests_unprocessed, 1, "{end}: the request");
    assert_eq!(sum.actions_unprocessed, 1, "{end}: the game action");
    assert_eq!(sum.control_frames_unprocessed, 1, "{end}: the heartbeat");
}

/// The verdicts a message brings — a pump's or the registry's
/// (`ServerClosed`), a room's kick (`ServerClosed` with `Kicked`), the
/// transport's refusal (`StreamRejected`) and the stop (`Shutdown`):
/// the frames behind each are counted, one per kind.
#[tokio::test]
async fn the_frames_behind_a_verdict_are_counted_by_kind() {
    let ends = [
        (
            "idle timeout",
            ConnIn::ServerClosed {
                cause: ServerClose::IdleTimeout,
                reason: "idle".into(),
            },
        ),
        (
            "kick",
            ConnIn::ServerClosed {
                cause: ServerClose::Kicked,
                reason: "kicked".into(),
            },
        ),
        (
            "stream rejected",
            ConnIn::StreamRejected {
                reason: "oversized".into(),
            },
        ),
        ("shutdown", ConnIn::Shutdown),
    ];
    for (end, msg) in ends {
        let mut c = Conn::open(64);
        c.auth().await;
        c.join(8).await;
        c.tell(msg).await;
        queue_one_of_each(&c).await;
        let sum = c.ended().await;
        assert_one_of_each(&sum, end);
    }
}

/// The room was destroyed: the frames behind the notice.
#[tokio::test]
async fn the_frames_behind_a_destroyed_room_are_counted() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.join(8).await;
    c.tell(ConnIn::RoomGone(RoomId(1))).await;
    queue_one_of_each(&c).await;
    let sum = c.ended().await;
    assert_eq!(sum.server_close, Some(ServerClose::RoomGone));
    assert_one_of_each(&sum, "room gone");
}

/// Four undefined base-band opcodes exhaust the violation budget (hard
/// weight 4, budget 16): the frames queued behind the fourth are counted
/// — and the four themselves were processed, so they are not.
#[tokio::test]
async fn the_frames_behind_the_violation_close_are_counted() {
    let mut c = Conn::open(64);
    c.auth().await;
    for _ in 0..4 {
        c.send(500, Vec::new()).await;
    }
    queue_one_of_each(&c).await;
    let sum = c.ended().await;
    assert_eq!(sum.server_close, Some(ServerClose::ViolationBudget));
    assert_eq!(sum.violations, 4);
    assert_one_of_each(&sum, "violation budget");
}

/// 64 pre-auth heartbeats spend the budget; the request after them
/// crosses it and is not processed, and the two game frames behind it
/// are never read.
#[tokio::test]
async fn the_frame_crossing_the_preauth_budget_and_those_behind_it_are_counted() {
    let c = Conn::open_deep(128);
    for _ in 0..64 {
        c.send(op::base::HEARTBEAT, heartbeat()).await;
    }
    c.send(op::base::RPC_REQ, vec![1]).await;
    c.send(GAME_OP, game_frame()).await;
    c.send(GAME_OP, game_frame()).await;
    let sum = c.ended().await;
    assert_eq!(sum.server_close, Some(ServerClose::PreauthBudget));
    assert_eq!(sum.requests_unprocessed, 1, "the crossing request");
    assert_eq!(sum.actions_unprocessed, 2, "the two behind it");
    assert_eq!(
        sum.control_frames_unprocessed, 0,
        "every heartbeat was read"
    );
    assert_eq!(
        sum.requests_no_room, 0,
        "the crossing request was not processed"
    );
}

/// The writer is gone when the heartbeat is answered: the session ends
/// as a dead outbound path, and the frames the actor looked through for a
/// pending verdict (`adopt_pending_close`) are counted, not discarded.
#[tokio::test]
async fn the_frames_behind_a_dead_outbound_path_are_counted() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.join(8).await;
    c.writer_gone();
    c.send(op::base::HEARTBEAT, heartbeat()).await;
    queue_one_of_each(&c).await;
    let sum = c.ended().await;
    assert_eq!(sum.server_close, Some(ServerClose::OutboundDead));
    assert_one_of_each(&sum, "outbound dead");
}

/// A client-side end leaves nothing behind: the reader's `Closed` is its
/// last message.
#[tokio::test]
async fn a_client_side_end_leaves_nothing_unprocessed() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.join(8).await;
    c.send(op::base::RPC_REQ, vec![1]).await;
    let sum = c.close().await;
    assert_eq!(sum.requests_unprocessed, 0);
    assert_eq!(sum.actions_unprocessed, 0);
    assert_eq!(sum.control_frames_unprocessed, 0);
}
