//! The room-lifecycle half of the registry sample: `rooms_created`,
//! `rooms_destroyed` and the `rooms` gauge.
//!
//! (`rooms_died` is the fourth member of this family and is tested where
//! its path lives — `tests/supervision.rs`, whose logics are built to
//! panic; see `supervision::counters`.)

use super::*;

/// `rooms_created` / `rooms_destroyed` are cumulative flow and `rooms`
/// is the table size — the room-side mirror of the connection test in
/// the parent module.
///
/// The destroy is also the `rooms_died` control: an ORDINARY destroy is
/// not a death, and the two counters share a report line. A destroy path
/// that also bumped `rooms_died` would have an operator hunting a game
/// logic panic that never happened (the field's doc calls non-zero
/// "game logic panicked somewhere").
#[tokio::test]
async fn room_creates_and_destroys_are_flow_while_rooms_is_the_table_size() {
    let (tx, mut metrics, handle) = start_observed();

    create_room(&tx, RoomId(1)).await;
    create_room(&tx, RoomId(2)).await;
    let s = latest(&mut metrics);
    assert_eq!(s.rooms_created, 2, "two rooms were created");
    assert_eq!(s.rooms_destroyed, 0);
    assert_eq!(s.rooms, 2, "and both are in the table");

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<RoomStatus>();
    tx.send(RegistryMsg::DestroyRoom {
        id: RoomId(1),
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    assert_eq!(
        tokio::time::timeout(WAIT, reply_rx)
            .await
            .expect("timed out")
            .expect("reply dropped"),
        RoomStatus::Destroyed
    );

    let s = latest(&mut metrics);
    assert_eq!(
        s.rooms_created, 2,
        "a destroy does not undo a create: the flow is cumulative"
    );
    assert_eq!(s.rooms_destroyed, 1, "one room was destroyed");
    assert_eq!(s.rooms, 1, "the gauge follows the table");
    assert_eq!(
        s.rooms_died, 0,
        "an ordinary destroy is NOT an unexpected death: `rooms_died` is \
         the 'game logic panicked' signal and must stay clean"
    );

    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not stop")
        .expect("registry task panicked");
}

/// Destroying an absent room is idempotent (`Ok(Absent)`) and must not
/// count: `rooms_destroyed` measures rooms that actually went away, and
/// a control plane that retries a destroy would otherwise inflate it.
#[tokio::test]
async fn a_destroy_of_an_absent_room_is_not_counted() {
    let (tx, mut metrics, handle) = start_observed();
    create_room(&tx, RoomId(1)).await;

    for _ in 0..2 {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<RoomStatus>();
        tx.send(RegistryMsg::DestroyRoom {
            id: RoomId(1),
            reply: reply_tx,
        })
        .await
        .expect("registry gone");
        let _ = tokio::time::timeout(WAIT, reply_rx)
            .await
            .expect("timed out")
            .expect("reply dropped");
    }

    let s = latest(&mut metrics);
    assert_eq!(
        s.rooms_destroyed, 1,
        "the retry found no room to destroy and must not be counted"
    );
    assert_eq!(s.rooms, 0);

    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not stop")
        .expect("registry task panicked");
}
