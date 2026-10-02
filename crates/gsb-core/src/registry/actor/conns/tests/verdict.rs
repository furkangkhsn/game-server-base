//! The verdict behind a close rides the detach the registry sends itself
//! when the connection's dispatcher is gone (BACKLOG F28, B61): the room
//! asks the policy with `ConnectionClosedBy(verdict)` there too.

use super::*;
use crate::conn::ServerClose;

#[tokio::test]
async fn a_gone_dispatcher_s_detach_carries_the_verdict() {
    let (causes_tx, mut causes) = mpsc::unbounded_channel();
    let factory: RoomFactory<(), (), (), ()> = Arc::new(move |_id, _cfg| BuiltRoom::Single {
        world: (),
        logic: Box::new(Logged(causes_tx.clone())),
    });
    let (tx, rx) = channel::<RegistryMsg>(64);
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics, _m) = mpsc::channel(64);
    let mut reg = Registry::new(rx, tx, factory, ticker, metrics, None, None, None);
    let (room, conn) = (RoomId(1), ConnectionId(7));
    let (reply, created) = oneshot::channel();
    let config = RoomConfig {
        id: room,
        ..Default::default()
    };
    reg.on_create_room(config, reply).await;
    created.await.expect("reply").expect("created");
    let (inbox, _inbox) = channel(8);
    reg.on_conn_opened(conn, inbox, None).await;
    let (out, _out) = channel(64);
    let (reply, seated) = oneshot::channel();
    reg.on_spawn_player(conn, room, out, String::new(), None, reply)
        .await;
    seated.await.expect("reply").expect("joined");
    match tokio::time::timeout(WAIT, reg.inbox.recv()).await {
        Ok(Some(RegistryMsg::SpawnDone {
            conn: c,
            room: r,
            entity: e,
            generation,
        })) => reg.on_spawn_done(c, r, e, generation).await,
        other => panic!("expected the join's settlement, got {other:?}"),
    }

    // The dispatcher is gone (as in the B61 test next door).
    let (serial, _live) = reg.conn_ops.remove(&conn).expect("a dispatcher");
    let (dead, _) = mpsc::channel(1);
    reg.conn_ops.insert(conn, (serial, dead));
    reg.on_conn_closed(conn, Some(ServerClose::WriteStall))
        .await;

    let ended = tokio::time::timeout(WAIT, causes.recv())
        .await
        .expect("on_disconnect ran")
        .expect("logic alive");
    assert_eq!(
        ended,
        (
            PlayerId(conn.0),
            DisconnectCause::ConnectionClosedBy(ServerClose::WriteStall)
        )
    );
}
