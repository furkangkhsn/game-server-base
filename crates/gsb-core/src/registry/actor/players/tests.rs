//! A join whose dispatcher is GONE (BACKLOG B63): its dead sender used to
//! stay in `conn_ops`, so every later join of the connection was refused
//! (`join_ops_dropped`) until it closed. The registry now replaces the
//! dead dispatcher and hands the join to the fresh one — after the leave
//! of any membership the table still records, so nothing is lost or
//! doubled.

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

/// What the room's logic saw, by player (= entity = admission order).
#[derive(Debug, PartialEq)]
enum Ev {
    Joined(u64),
    Left(u64),
    Disconnected(u64),
}

/// Admits players 1, 2, … and logs every membership event.
struct Counted {
    next: u64,
    log: mpsc::UnboundedSender<Ev>,
}

impl GameLogic<()> for Counted {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7E72
    }
    fn private_op(&self) -> u16 {
        0x7E73
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
    fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> Admission {
        self.next += 1;
        let _ = self.log.send(Ev::Joined(self.next));
        Admission {
            player: PlayerId(self.next),
            entity: self.next as EntityId,
        }
    }
    fn on_leave(&mut self, _w: &mut (), p: PlayerId) {
        let _ = self.log.send(Ev::Left(p.0));
    }
    fn on_disconnect_with(
        &mut self,
        _w: &mut (),
        p: PlayerId,
        _identity: &str,
        _cause: DisconnectCause,
    ) -> Detach {
        let _ = self.log.send(Ev::Disconnected(p.0));
        Detach::Despawn
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

impl RoomLogic<()> for Counted {}

type Reg = Registry<(), (), (), ()>;

const ROOM: RoomId = RoomId(1);
const CONN: ConnectionId = ConnectionId(7);

/// A registry with room 1 created and connection 7 open.
async fn setup() -> (Reg, mpsc::UnboundedReceiver<Ev>) {
    let (log, events) = mpsc::unbounded_channel();
    let factory: RoomFactory<(), (), (), ()> = Arc::new(move |_id, _cfg| BuiltRoom::Single {
        world: (),
        logic: Box::new(Counted {
            next: 0,
            log: log.clone(),
        }),
    });
    let (tx, rx) = channel::<RegistryMsg>(64);
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics, _m) = mpsc::channel(64);
    let mut reg = Registry::new(rx, tx, factory, ticker, metrics, None, None, None);
    let (reply, created) = oneshot::channel();
    let config = RoomConfig {
        id: ROOM,
        ..Default::default()
    };
    reg.on_create_room(config, reply).await;
    created.await.expect("reply").expect("created");
    let (inbox, _inbox) = channel(8);
    reg.on_conn_opened(CONN, inbox).await;
    (reg, events)
}

/// Join room 1 and wait for the connection's seat.
async fn join(reg: &mut Reg) -> EntityId {
    let (out, _out) = channel(64);
    let (reply, seated) = oneshot::channel();
    reg.on_spawn_player(CONN, ROOM, out, String::new(), reply)
        .await;
    let seat = tokio::time::timeout(WAIT, seated).await.expect("answered");
    seat.expect("the join was dropped: a gone dispatcher refused it")
        .expect("joined")
        .entity
}

/// The next registry message, which must be this join's settlement.
async fn settle(reg: &mut Reg, want: EntityId) {
    match tokio::time::timeout(WAIT, reg.inbox.recv()).await {
        Ok(Some(RegistryMsg::SpawnDone {
            conn,
            room,
            entity,
            generation,
        })) => {
            assert_eq!((conn, room, entity), (CONN, ROOM, want));
            reg.on_spawn_done(conn, room, entity, generation).await;
        }
        other => panic!("expected the join's settlement, got {other:?}"),
    }
}

#[tokio::test]
async fn a_gone_dispatcher_is_replaced_and_the_join_goes_through() {
    let (mut reg, mut events) = setup().await;
    // A dispatcher whose task has ended: its queue is closed.
    let (dead, _) = mpsc::channel(1);
    reg.conn_ops.insert(CONN, dead);

    let entity = join(&mut reg).await;
    assert_eq!(reg.reg_join_ops_dropped, 0, "the join was not refused");
    settle(&mut reg, entity).await;
    assert_eq!(events.recv().await, Some(Ev::Joined(1)));
    let info = reg.conns.get(&CONN).expect("row");
    assert_eq!((info.room, info.entity), (Some(ROOM), Some(entity)));
    assert!(
        !reg.conn_ops[&CONN].is_closed(),
        "the fresh dispatcher took the dead one's slot"
    );
}

#[tokio::test]
async fn a_membership_the_gone_dispatcher_held_is_left_before_the_retried_join() {
    let (mut reg, mut events) = setup().await;
    let first = join(&mut reg).await;
    settle(&mut reg, first).await;
    assert_eq!(events.recv().await, Some(Ev::Joined(1)));

    // The dispatcher dies holding the membership the table records (a
    // leave it accepted but never ran leaves exactly this). The live one
    // is kept open, so its own end cannot run before the assertions.
    let live = reg.conn_ops.remove(&CONN).expect("a dispatcher");
    let (dead, _) = mpsc::channel(1);
    reg.conn_ops.insert(CONN, dead);

    let second = join(&mut reg).await;
    assert_ne!(second, first);
    match tokio::time::timeout(WAIT, reg.inbox.recv()).await {
        Ok(Some(RegistryMsg::LeaveDone { conn, room })) => assert_eq!((conn, room), (CONN, ROOM)),
        other => panic!("expected the old membership's leave first, got {other:?}"),
    }
    settle(&mut reg, second).await;
    assert_eq!(events.recv().await, Some(Ev::Left(1)), "left before");
    assert_eq!(events.recv().await, Some(Ev::Joined(2)), "the new join");

    // The old dispatcher's late end finds nothing left to end.
    drop(live);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        events.try_recv().is_err(),
        "no membership doubled or ended twice"
    );
}
