//! The dispatcher driven with raw ops against a hand-played room (BACKLOG
//! B64): a `Leave` for a room the connection is not in must leave its
//! membership intact. The connection actor only ever leaves the room its
//! table row records, so this is reachable only through raw ops.

use tokio::sync::oneshot;

use crate::channel::channel;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::registry::actor::Registry;
use crate::registry::{RegistryMsg, RoomHandle, RoomOp};
use crate::room::{Action, RoomControl};

type Reg = Registry<(), (), (), ()>;

const CONN: ConnectionId = ConnectionId(3);
const ROOM: RoomId = RoomId(1);
const OTHER: RoomId = RoomId(2);
const ENTITY: EntityId = 5;

#[tokio::test]
async fn a_leave_for_another_room_keeps_the_membership() {
    let (registry, mut reports) = channel::<RegistryMsg>(16);
    let (control, mut room) = channel::<RoomControl>(8);
    let ops = Reg::spawn_conn_ops(CONN, registry, None);
    let (out, _out) = channel(8);
    let (reply, seated) = oneshot::channel();
    let join = RoomOp::Join {
        room: ROOM,
        handle: RoomHandle::Single(control),
        shard: None,
        generation: 0,
        epoch: 1,
        out,
        identity: String::new(),
        input_rate: None,
        reply,
    };
    assert!(ops.try_send(join).is_ok());
    let (actions, _actions) = channel::<Action>(8);
    match room.recv().await {
        Some(RoomControl::Join { conn, reply, .. }) => {
            assert_eq!(conn, CONN);
            let _ = reply.send(Ok((ENTITY, actions)));
        }
        other => panic!("expected the join, got {other:?}"),
    }
    assert_eq!(seated.await.expect("reply").expect("joined").entity, ENTITY);
    assert!(matches!(
        reports.recv().await,
        Some(RegistryMsg::SpawnDone { entity: ENTITY, .. })
    ));

    // A leave for a room this connection is not in: answered as ever
    // (nothing to the room, no `LeaveDone`) — and forgets nothing, so
    // the close behind it still detaches the membership.
    assert!(ops.try_send(RoomOp::Leave { room: OTHER }).is_ok());
    assert!(ops.try_send(RoomOp::Close).is_ok());

    match reports.recv().await {
        Some(RegistryMsg::DetachDone { conn, room }) => assert_eq!((conn, room), (CONN, ROOM)),
        other => panic!("the close found no membership to detach, got {other:?}"),
    }
    assert!(matches!(
        reports.recv().await,
        Some(RegistryMsg::OpsClosed { conn: CONN })
    ));
    match room.try_recv() {
        Ok(RoomControl::Detach { conn, entity, .. }) => assert_eq!((conn, entity), (CONN, ENTITY)),
        other => panic!("expected the detach alone, got {other:?}"),
    }
    assert!(room.try_recv().is_err(), "nothing for the mismatched leave");
}
