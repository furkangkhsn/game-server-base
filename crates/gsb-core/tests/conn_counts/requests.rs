//! The RPC ledger's two connection-side edges (BACKLOG B55). Before, a
//! request dropped on a full action channel was counted in
//! `actions_dropped` with the game input, and a request received outside
//! any room only in `violations` with every other violation: neither
//! could be read out for the ledger. Now each has its own counter, and
//! `actions_dropped` counts what its name says — game actions.

use super::*;
use rig::{Conn, GAME_OP, game_frame};

/// An action channel of one slot, never read: the first frame fills it,
/// every later one is dropped — a request into `requests_dropped_full`,
/// a game action into `actions_dropped`, never the other.
#[tokio::test]
async fn a_request_dropped_on_a_full_channel_is_counted_apart_from_actions() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.join(1).await;
    c.send(GAME_OP, game_frame()).await; // fills the channel
    c.send(op::base::RPC_REQ, vec![1]).await;
    c.send(op::base::RPC_REQ, vec![2]).await;
    c.send(GAME_OP, game_frame()).await;
    let sum = c.close().await;
    assert_eq!(sum.requests_dropped_full, 2, "the two requests");
    assert_eq!(sum.actions_dropped, 1, "the one game action, alone");
    assert_eq!(sum.requests_no_room, 0);
}

/// A request before any join and one after the leave: each answered
/// `ERROR 6` and counted as a violation (unchanged), and each counted
/// once as a no-room request; a game frame outside a room is a
/// violation only.
#[tokio::test]
async fn a_request_outside_any_room_is_counted_for_the_ledger() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.send(op::base::RPC_REQ, vec![1]).await;
    let err = c.until(op::base::ERROR).await;
    let code = base::Error::decode(&err.payload[..])
        .expect("decodes")
        .code();
    assert_eq!(code, base::ErrorCode::NotInRoom);
    c.join(8).await;
    c.send(op::base::LEAVE_ROOM_REQ, Vec::new()).await;
    c.until(op::base::LEAVE_ROOM_RESULT).await;
    c.send(op::base::RPC_REQ, vec![2]).await;
    c.send(GAME_OP, game_frame()).await;
    let sum = c.close().await;
    assert_eq!(
        sum.requests_no_room, 2,
        "one before the join, one after the leave"
    );
    assert_eq!(sum.violations, 3, "the violation accounting is unchanged");
    assert_eq!(sum.requests_dropped_full, 0);
}
