//! The races around the default action's settlement (BACKLOG B40): a
//! join elsewhere that overtakes a late leave request, and a notice that
//! reaches a connection already in a newer membership.

use super::*;
use leave_table::{config, join, leave, members, open};
use rig::Client;

/// A join into ANOTHER room while the row still holds a membership the
/// registry was never told the end of (the leave request still in
/// flight): the ended membership's grid slot comes back at the join —
/// the late request then finds the row moved on and does nothing.
#[tokio::test]
async fn a_join_elsewhere_settles_an_unreported_end() {
    let (disc, _d) = mpsc::unbounded_channel();
    let (tx, _m) = start(logic::factory(Detach::Despawn, true, disc));
    create(
        &tx,
        RoomConfig {
            max_players: Some(1),
            ..config()
        },
    )
    .await;
    create(
        &tx,
        RoomConfig {
            id: RoomId(2),
            ..config()
        },
    )
    .await;
    let _one = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await;
    let (reply, rx) = oneshot::channel();
    let (out, _out) = mpsc::channel(64);
    tx.send(RegistryMsg::SpawnPlayer {
        conn: ConnectionId(1),
        room: RoomId(2),
        out,
        identity: "ana".into(),
        reply,
        claims: None,
    })
    .await
    .expect("registry alive");
    tokio::time::timeout(WAIT, rx)
        .await
        .expect("in time")
        .expect("reply")
        .expect("joined room 2");
    tx.send(leave(1, entity, false)).await.expect("sent (late)");

    let _two = open(&tx, 2).await;
    join(&tx, 2, "bo").await;
    assert_eq!(status(&tx, RoomId(1)).await, members(1));
    assert_eq!(status(&tx, RoomId(2)).await, members(1));
}

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
