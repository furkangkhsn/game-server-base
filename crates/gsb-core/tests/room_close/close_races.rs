//! The registry's side of a close request's race with the park it names
//! (BACKLOG B41), driven with the raw messages in the order a room sends
//! them when the park ENDS while the request still waits behind a full
//! mailbox: the hold's `DetachDespawned` (phase 0c) goes ahead of the
//! request (phase 0d). The registry cannot tell that early report from a
//! stale echo — the row is not detached yet — so it drops it, and the
//! request alone decides what is left: the room must re-check `parked`
//! when the request leaves (`room/tests/idle/afk/races.rs`).

use super::*;
use leave_table::{config, join, members, open};

fn close(conn: u64, entity: EntityId, parked: bool) -> RegistryMsg {
    RegistryMsg::CloseConn(CloseRequest {
        conn: ConnectionId(conn),
        room: RoomId(1),
        entity,
        parked,
        cause: ServerClose::IdleInput,
        reason: "input idle: test".into(),
    })
}

fn report(conn: u64) -> RegistryMsg {
    RegistryMsg::DetachDespawned {
        conn: ConnectionId(conn),
        room: RoomId(1),
    }
}

/// Both shapes of room. Member 1's request still says `parked` after its
/// park's report went ahead: its row is marked detached with nothing left
/// to release it, and the room keeps counting it. Member 2's request was
/// re-checked (`parked = false`): the membership is released.
#[tokio::test]
async fn a_report_ahead_of_the_close_leaves_it_to_the_rechecked_flag() {
    for sharded in [false, true] {
        let (disc, _d) = mpsc::unbounded_channel();
        let (tx, _m) = start(logic::factory(Detach::Despawn, sharded, disc));
        create(&tx, config()).await;
        let _one = open(&tx, 1).await;
        let _two = open(&tx, 2).await;
        let e1 = join(&tx, 1, "ana").await;
        let e2 = join(&tx, 2, "bo").await;
        assert_eq!(status(&tx, RoomId(1)).await, members(2));
        for msg in [
            report(1),
            close(1, e1, true),
            report(2),
            close(2, e2, false),
        ] {
            tx.send(msg).await.expect("registry alive");
        }
        assert_eq!(
            status(&tx, RoomId(1)).await,
            members(1),
            "sharded={sharded}: only the stale `parked` close holds a slot"
        );
    }
}
