//! A verdict the registry hands a connection is in the connection's
//! inbox the moment the registry has handled it (BACKLOG F57): posted in
//! place, so it is queued ahead of anything the registry sends after —
//! the stop's `ConnIn::Shutdown` above all. Sent from a spawned task, it
//! could land behind that notice (a lost verdict, F56) or, once the
//! connection had closed its inbox, be refused uncounted.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::channel::{Inbox, Mailbox, channel};
use crate::conn::{ConnIn, ServerClose};
use crate::id::{ConnectionId, RoomId};
use crate::registry::actor::Registry;
use crate::registry::{CloseRequest, ConnInfo, RegistryMsg, RoomFactory};
use crate::ticker::Ticker;

mod refused;

type Reg = Registry<(), (), (), ()>;

const CONN: ConnectionId = ConnectionId(3);

/// A registry (not running yet: the test calls its arms) with a
/// connection cap of `cap`, its mailbox, and a connection row for `CONN`
/// in room 1 whose inbox is returned.
fn registry(cap: Option<u64>) -> (Reg, Mailbox<RegistryMsg>, Inbox<ConnIn>) {
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room is created here"));
    let (tx, rx) = channel::<RegistryMsg>(8);
    let (metrics, _metrics) = mpsc::channel(8);
    let mut reg: Reg = Registry::new(rx, tx.clone(), factory, ticker, metrics, cap, None, None);
    let (inbox, conn) = channel::<ConnIn>(8);
    reg.conns.insert(
        CONN,
        ConnInfo {
            room: Some(RoomId(1)),
            entity: Some(7),
            inbox: Some(inbox),
            authed: true,
            ..ConnInfo::default()
        },
    );
    (reg, tx, conn)
}

/// A room's kick: in the connection's inbox as soon as the registry has
/// handled it, and the stop's notice behind it.
#[tokio::test]
async fn a_close_verdict_is_queued_ahead_of_the_stop() {
    let (mut reg, tx, mut conn) = registry(None);
    reg.on_close_conn(CloseRequest {
        conn: CONN,
        room: RoomId(1),
        entity: 7,
        parked: false,
        cause: ServerClose::Kicked,
        reason: "kicked: afk".into(),
    });
    assert!(
        matches!(
            conn.try_recv(),
            Ok(ConnIn::ServerClosed {
                cause: ServerClose::Kicked,
                ..
            })
        ),
        "queued in place"
    );
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    let next = tokio::time::timeout(Duration::from_secs(5), conn.recv()).await;
    assert!(matches!(next, Ok(Some(ConnIn::Shutdown))), "{next:?}");
}

/// A connection refused at the cap is told in place too, and a room's
/// end reaches its members the same way.
#[tokio::test]
async fn a_cap_refusal_and_a_room_gone_are_queued_in_place() {
    let (mut reg, _tx, mut member) = registry(Some(1));
    let (late, mut refused) = channel::<ConnIn>(8);
    reg.on_conn_opened(ConnectionId(4), late).await;
    assert!(matches!(
        refused.try_recv(),
        Ok(ConnIn::ServerClosed {
            cause: ServerClose::ConnCap,
            ..
        })
    ));
    reg.notify_room_gone(RoomId(1));
    assert!(matches!(member.try_recv(), Ok(ConnIn::RoomGone(RoomId(1)))));
}
