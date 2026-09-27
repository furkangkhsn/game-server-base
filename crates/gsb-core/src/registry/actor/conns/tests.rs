//! A close whose dispatcher is GONE (BACKLOG B61): the task that held the
//! membership died with it, so nothing will drain a queue or run its
//! close. The registry detaches the table's affiliation itself — the
//! game's `on_disconnect` runs once, as a closed connection, and the
//! despawn's report comes back to release the row. (A full queue, the
//! other refusal, is covered end to end in `tests/room_close/close_op.rs`.)

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::channel::channel;
use crate::id::{ConnectionId, EntityId, PlayerId, RoomId};
use crate::registry::actor::Registry;
use crate::registry::{BuiltRoom, RegistryMsg, RoomFactory};
use crate::room::{
    Action, Admission, Detach, DisconnectCause, GameLogic, RoomConfig, RoomLogic, TickCtx,
};
use crate::shard::BorderRecord;
use crate::ticker::Ticker;

const WAIT: Duration = Duration::from_secs(5);

type Causes = mpsc::UnboundedSender<(PlayerId, DisconnectCause)>;

/// Player = entity = the conn id; logs every disconnect's cause.
struct Logged(Causes);

impl GameLogic<()> for Logged {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7E70
    }
    fn private_op(&self) -> u16 {
        0x7E71
    }
    fn group_of(&self, _w: &(), _p: PlayerId) {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(c.0),
            entity: c.0 as EntityId,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn on_disconnect_with(
        &mut self,
        _w: &mut (),
        player: PlayerId,
        _identity: &str,
        cause: DisconnectCause,
    ) -> Detach {
        let _ = self.0.send((player, cause));
        Detach::Despawn
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

impl RoomLogic<()> for Logged {}

#[tokio::test]
async fn a_gone_dispatcher_s_close_detaches_from_the_table() {
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
    reg.on_conn_opened(conn, inbox).await;
    let (out, _out) = channel(64);
    let (reply, seated) = oneshot::channel();
    reg.on_spawn_player(conn, room, out, String::new(), reply)
        .await;
    let entity = seated.await.expect("reply").expect("joined").entity;
    match tokio::time::timeout(WAIT, reg.inbox.recv()).await {
        Ok(Some(RegistryMsg::SpawnDone {
            conn: c,
            room: r,
            entity: e,
            generation,
        })) => reg.on_spawn_done(c, r, e, generation).await,
        other => panic!("expected the join's settlement, got {other:?}"),
    }

    // The dispatcher is gone: the registry holds a sender whose task has
    // ended. (The live one is kept open, so it never runs its own close
    // before the assertions below.)
    let live = reg.conn_ops.remove(&conn).expect("a dispatcher");
    let (dead, _) = mpsc::channel(1);
    reg.conn_ops.insert(conn, dead);
    reg.on_conn_closed(conn).await;
    assert_eq!(reg.reg_close_ops_dropped, 1, "refused, still counted");

    let ended = tokio::time::timeout(WAIT, causes.recv())
        .await
        .expect("on_disconnect never ran: the gone dispatcher lost the detach")
        .expect("logic alive");
    assert_eq!(ended, (PlayerId(conn.0), DisconnectCause::ConnectionClosed));
    match tokio::time::timeout(WAIT, reg.inbox.recv()).await {
        Ok(Some(RegistryMsg::DetachDespawned { conn: c, room: r })) => {
            assert_eq!((c, r, entity), (conn, room, conn.0 as EntityId));
        }
        other => panic!("expected the despawn's report, got {other:?}"),
    }

    // The old dispatcher's own close, late, finds nothing left to end.
    drop(live);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(causes.try_recv().is_err(), "on_disconnect ran once");
}
