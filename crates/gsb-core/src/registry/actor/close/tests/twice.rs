//! One session, one lost verdict (BACKLOG F60). A verdict the registry
//! sends a connection whose inbox is FULL waits in a spawned sender; at
//! the stop the stop's notice can reach the connection first. Before
//! F60 the refused fallback was counted where it was refused (F58) and
//! the connection's end counted the first verdict behind the stop (F56):
//! two counts for one session — and a refusal after the stop was counted
//! even when the session had ended on its own (the client's end, another
//! verdict), which loses nothing.
//!
//! Since F60 the stop's notice names the verdict left waiting
//! (`ConnIn::ShutdownOvertaking`, `refused.rs` locks that the registry
//! sends it) and only the connection that reads it counts. The order the
//! two spawned senders reach the inbox in is the runtime's; each test
//! queues by hand what the connection reads first.

use std::net::SocketAddr;

use super::*;
use crate::conn::ConnectionActor;
use crate::metrics::{MetricsEvent, VerdictsLost};

/// A registry (not running yet), a connection row for `CONN` whose inbox
/// holds exactly `queued` (and is full), the inbox's receiving end, and
/// the metrics channel both actors report to.
fn rig(
    queued: Vec<ConnIn>,
) -> (
    Reg,
    Mailbox<RegistryMsg>,
    Inbox<ConnIn>,
    mpsc::Sender<MetricsEvent>,
    mpsc::Receiver<MetricsEvent>,
) {
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room is created here"));
    let (tx, rx) = channel::<RegistryMsg>(8);
    let (metrics, metrics_rx) = mpsc::channel(64);
    let mut reg: Reg = Registry::new(
        rx,
        tx.clone(),
        factory,
        ticker,
        metrics.clone(),
        None,
        None,
        None,
    );
    let (inbox, conn) = channel::<ConnIn>(queued.len());
    for msg in queued {
        inbox.try_send(msg).expect("a slot");
    }
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
    (reg, tx, conn, metrics, metrics_rx)
}

fn kick() -> CloseRequest {
    CloseRequest {
        conn: CONN,
        room: RoomId(1),
        entity: 7,
        parked: false,
        cause: ServerClose::Kicked,
        reason: "kicked: afk".into(),
    }
}

fn idle_timeout() -> ConnIn {
    ConnIn::ServerClosed {
        cause: ServerClose::IdleTimeout,
        reason: "idle timeout".into(),
    }
}

/// The kick waits in the registry's fallback, the registry stops, then
/// the connection actor runs over `inbox` to its end.
async fn kick_stop_and_end(
    mut reg: Reg,
    tx: Mailbox<RegistryMsg>,
    inbox: Inbox<ConnIn>,
    metrics: mpsc::Sender<MetricsEvent>,
) {
    reg.on_close_conn(kick());
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    let (registry, _registry_rx) = channel::<RegistryMsg>(8);
    let (out, _out_rx) = channel(8);
    let actor = ConnectionActor::new(
        CONN,
        SocketAddr::from(([127, 0, 0, 1], 45_060)),
        Arc::new(gsb_protocol::base_table()),
        registry,
        inbox,
        out,
        metrics,
        None,
    );
    tokio::time::timeout(Duration::from_secs(5), actor.run())
        .await
        .expect("the actor ends");
}

/// Every verdict counted lost, read to the channel's end (every producer
/// — registry, actor, spawned senders — has dropped its sender).
async fn lost(mut metrics: mpsc::Receiver<MetricsEvent>) -> VerdictsLost {
    let mut lost = VerdictsLost::default();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), metrics.recv())
            .await
            .expect("every producer ends");
        match ev {
            Some(MetricsEvent::VerdictsLost(l)) => lost.add(&l),
            Some(_) => {}
            None => return lost,
        }
    }
}

/// F60 (a): the stop's notice reached the connection first, a pump's
/// verdict sits behind it, and the registry's kick is still in its
/// fallback. The session lost ONE verdict: the kick the stop overtook
/// (decided before the stop), not the pump's behind it.
#[tokio::test]
async fn two_verdicts_behind_the_stop_are_one_lost_verdict() {
    let stop = ConnIn::ShutdownOvertaking(ServerClose::Kicked);
    let (reg, tx, inbox, metrics, metrics_rx) = rig(vec![stop, idle_timeout()]);
    kick_stop_and_end(reg, tx, inbox, metrics).await;
    let lost = lost(metrics_rx).await;
    assert_eq!(lost.closes.total(), 1, "one per session: {lost:?}");
    assert_eq!(lost.closes.get(ServerClose::Kicked), 1, "{lost:?}");
}

/// The overtaking stop alone (the kick refused at the end): the kick is
/// the lost verdict, once.
#[tokio::test]
async fn the_verdict_the_stop_overtook_is_lost_once() {
    let stop = ConnIn::ShutdownOvertaking(ServerClose::Kicked);
    let (reg, tx, inbox, metrics, metrics_rx) = rig(vec![stop]);
    kick_stop_and_end(reg, tx, inbox, metrics).await;
    let lost = lost(metrics_rx).await;
    assert_eq!(lost.closes.get(ServerClose::Kicked), 1, "{lost:?}");
    assert_eq!(lost.closes.total(), 1, "{lost:?}");
}

/// F60 (b): the client ended the session before the kick reached it;
/// the kick's fallback is refused after the stop. Nothing was lost.
#[tokio::test]
async fn a_verdict_refused_by_a_client_end_is_not_lost() {
    let closed = ConnIn::Closed {
        reason: "peer left".into(),
    };
    let (reg, tx, inbox, metrics, metrics_rx) = rig(vec![closed]);
    kick_stop_and_end(reg, tx, inbox, metrics).await;
    assert!(lost(metrics_rx).await.is_empty());
}

/// The same when another verdict ended the session first: it was booked
/// (`server_closes`), and the session books one reason.
#[tokio::test]
async fn a_verdict_refused_behind_another_verdict_is_not_lost() {
    let (reg, tx, inbox, metrics, metrics_rx) = rig(vec![idle_timeout()]);
    kick_stop_and_end(reg, tx, inbox, metrics).await;
    assert!(lost(metrics_rx).await.is_empty());
}
