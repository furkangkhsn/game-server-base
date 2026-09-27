//! The races around the default action's settlement (BACKLOG B40): a
//! notice that reaches a connection already in a newer membership.

use super::*;
use rig::Client;

/// The connection's side of the notice: one that arrives while the
/// connection holds an OPEN action channel (a newer membership) is stale
/// and ignored — the connection is still in the room.
#[tokio::test(start_paused = true)]
async fn a_stale_notice_leaves_a_live_membership_alone() {
    let (disc, _d) = mpsc::unbounded_channel();
    let reg = rig::registry(
        logic::factory(Detach::Despawn, false, disc),
        AfkAction::LeaveRoom,
        None,
    )
    .await;
    let mut c = Client::login(&reg, 1, "ana").await;
    c.inbox
        .send(ConnIn::LeftRoom { room: RoomId(1) })
        .await
        .expect("actor alive");
    c.send(FrameBody::new(op::base::LEAVE_ROOM_REQ, Vec::new()))
        .await;
    c.expect(op::base::LEAVE_ROOM_RESULT).await;
}
