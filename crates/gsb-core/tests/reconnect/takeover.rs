//! A newer session of an identity whose old session is still LIVE at the
//! registry (BACKLOG F32): the reconnect outran the old socket's close —
//! a half-open socket, or a close still on its way. The registry closes
//! the old socket (ERROR 9, as ever) and HANDS the membership over: the
//! room runs the old session's detach and the new session resumes the
//! entity. It used to LEAVE the old membership, so the returning player
//! lost its entity to a fresh join (`loadgen_churn_smoke`'s `resumed=0`).

use super::*;

/// The old socket's notice: ERROR 9, superseded.
async fn told_superseded(inbox: &mut mpsc::Receiver<ConnIn>) {
    match tokio::time::timeout(WAIT, inbox.recv()).await {
        Ok(Some(ConnIn::ServerClosed { cause, .. })) => {
            assert_eq!(cause, gsb_core::conn::ServerClose::Superseded)
        }
        other => panic!("expected ServerClosed for the superseded socket: {other:?}"),
    }
}

#[tokio::test]
async fn a_newer_session_takes_the_live_one_over() {
    let (tx, handle) = start_registry(parking_factory());
    let room = RoomId(73);
    create_room(&tx, reg_config(room)).await.expect("create");
    let mut old = open_conn(&tx, ConnectionId(1)).await;
    let first = spawn_as(&tx, ConnectionId(1), room, "ana")
        .await
        .expect("first session joins");

    // c2 arrives before the registry saw c1 close.
    let _new = open_conn(&tx, ConnectionId(2)).await;
    let second = spawn_as(&tx, ConnectionId(2), room, "ana")
        .await
        .expect("the newer session joins");
    assert_eq!(second, first, "the SAME entity: handed over, not left");
    told_superseded(&mut old).await;
    let one = RoomStatus::Running { members: 1 };
    assert_eq!(status(&tx, room).await, one);

    // The old socket's close, late (a lower bound, not a window): its
    // detach finds nothing to park, the member stays.
    close_conn(&tx, ConnectionId(1)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(status(&tx, room).await, one);

    // The next drop parks the entity; the next session resumes it.
    close_conn(&tx, ConnectionId(2)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _third = open_conn(&tx, ConnectionId(3)).await;
    let third = spawn_as(&tx, ConnectionId(3), room, "ana")
        .await
        .expect("resume");
    assert_eq!(third, first);
    assert_eq!(status(&tx, room).await, one);

    stop_registry(tx, handle).await;
}

/// The grid's registry-side member count (what enforces `max_players` on
/// a sharded room) stays exact through a handover: the handed-over
/// membership is one member, and the old socket's late close releases
/// nothing more.
#[tokio::test]
async fn a_sharded_handover_keeps_the_member_count_exact() {
    let (tx, handle) = start_registry(expiring_sharded_factory(Duration::from_secs(3600)));
    let room = RoomId(74);
    let cfg = RoomConfig {
        max_players: Some(2),
        ..reg_config(room)
    };
    create_room(&tx, cfg).await.expect("create");
    let mut old = open_conn(&tx, ConnectionId(1)).await;
    let first = spawn_as(&tx, ConnectionId(1), room, "ana")
        .await
        .expect("first session joins");
    let _new = open_conn(&tx, ConnectionId(2)).await;
    let second = spawn_as(&tx, ConnectionId(2), room, "ana")
        .await
        .expect("the newer session joins");
    assert_eq!(second, first, "the SAME entity: handed over, not left");
    told_superseded(&mut old).await;
    close_conn(&tx, ConnectionId(1)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // One member so far: a second identity fits, a third does not.
    let _bob = open_conn(&tx, ConnectionId(3)).await;
    spawn_as(&tx, ConnectionId(3), room, "bob")
        .await
        .expect("bob fits under the cap");
    let _carl = open_conn(&tx, ConnectionId(4)).await;
    match spawn_as(&tx, ConnectionId(4), room, "carl").await {
        Err(CoreError::RoomFull(id)) => assert_eq!(id, room.0),
        other => panic!("expected RoomFull at the cap, got {other:?}"),
    }

    stop_registry(tx, handle).await;
}
