//! A verdict whose connection's inbox was FULL goes from a spawned
//! sender (BACKLOG F58). If the connection ends before a slot frees —
//! at the stop, the stop's own notice can reach it first — the send is
//! refused: counted then, in `close_verdicts_lost`, when the registry had
//! already stopped (the verdict was decided before the stop, and the
//! stop kept it from the client). A refusal while the registry runs is
//! a connection that ended on its own: nothing lost, nothing counted.

use super::*;
use crate::metrics::{MetricsEvent, VerdictsLost};

/// A registry (not running yet) whose metrics channel is returned, and
/// a connection row for `CONN` in room 1 whose inbox holds ONE message
/// and is already full: the connection is not reading it.
fn registry_full() -> (
    Reg,
    Mailbox<RegistryMsg>,
    Inbox<ConnIn>,
    mpsc::Receiver<MetricsEvent>,
) {
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room is created here"));
    let (tx, rx) = channel::<RegistryMsg>(8);
    let (metrics, metrics_rx) = mpsc::channel(64);
    let mut reg: Reg = Registry::new(rx, tx.clone(), factory, ticker, metrics, None, None, None);
    let (inbox, conn) = channel::<ConnIn>(1);
    inbox
        .try_send(ConnIn::LeftRoom { room: RoomId(2) })
        .expect("the one slot");
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
    (reg, tx, conn, metrics_rx)
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

/// The connection ends: its inbox closes (as its `abandon_inbox` does)
/// and what was queued is drained.
fn end(conn: &mut Inbox<ConnIn>) {
    conn.close();
    while conn.try_recv().is_ok() {}
}

/// Every verdict counted lost, read to the channel's end: the registry
/// is gone and every spawned sender has finished (each holds a metrics
/// sender until then).
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

/// The kick meets the full inbox and waits in a spawned sender; the
/// registry stops; the connection ends before a slot frees: the refused
/// kick is counted, once, under its reason.
#[tokio::test]
async fn a_verdict_refused_after_the_stop_is_counted() {
    let (mut reg, tx, mut conn, metrics) = registry_full();
    reg.on_close_conn(kick());
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    end(&mut conn);
    let lost = lost(metrics).await;
    assert_eq!(lost.closes.get(ServerClose::Kicked), 1, "{lost:?}");
    assert_eq!(lost.closes.total(), 1, "{lost:?}");
}

/// The same refusal while the registry still runs: the connection ended
/// on its own before the kick reached it — nothing counted, then or at
/// the later stop.
#[tokio::test]
async fn a_verdict_refused_while_the_registry_runs_is_not_counted() {
    let (mut reg, tx, mut conn, metrics) = registry_full();
    reg.on_close_conn(kick());
    end(&mut conn);
    // Let the spawned sender meet the closed inbox before the stop.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    assert!(lost(metrics).await.is_empty());
}

/// A destroyed room's notice waits the same way: refused after the
/// stop, it is counted under `room_gone`.
#[tokio::test]
async fn a_room_gone_refused_after_the_stop_is_counted() {
    let (mut reg, tx, mut conn, metrics) = registry_full();
    reg.notify_room_gone(RoomId(1));
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    end(&mut conn);
    let lost = lost(metrics).await;
    assert_eq!(lost.closes.get(ServerClose::RoomGone), 1, "{lost:?}");
    assert_eq!(lost.closes.total(), 1, "{lost:?}");
}

/// What a lost message costs, shared by the connection's end and the
/// registry's refused fallback: a verdict is its reason; a frame, the
/// client's end, a membership notice and the stop are none.
#[test]
fn a_message_is_a_verdict_only_when_the_server_decided_the_end() {
    let closed = ConnIn::ServerClosed {
        cause: ServerClose::Superseded,
        reason: String::new(),
    };
    assert_eq!(closed.verdict(), Some(ServerClose::Superseded));
    assert_eq!(
        ConnIn::RoomGone(RoomId(1)).verdict(),
        Some(ServerClose::RoomGone)
    );
    let rejected = ConnIn::StreamRejected {
        reason: String::new(),
    };
    assert_eq!(rejected.verdict(), Some(ServerClose::StreamRejected));
    let frame = ConnIn::Frame(gsb_protocol::FrameBody::new(1, Vec::new()));
    let client = ConnIn::Closed {
        reason: String::new(),
    };
    let left = ConnIn::LeftRoom { room: RoomId(1) };
    for msg in [frame, client, left, ConnIn::Shutdown] {
        assert_eq!(msg.verdict(), None, "{msg:?}");
    }
}
