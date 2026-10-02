//! A verdict whose connection's inbox was FULL goes from a spawned
//! sender (BACKLOG F58). At the stop, the stop's own notice can reach the
//! connection first. Since F60 the refusing side counts nothing (it
//! cannot know how the session ended): the stop's notice names the
//! verdict left waiting (`ConnIn::ShutdownOvertaking`), and the
//! connection that reads it first counts it, once (`twice.rs`).

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
/// registry stops: its notice names the kick (F60), and whichever of the
/// two the connection reads first, it reads both.
#[tokio::test]
async fn the_stop_names_a_verdict_left_waiting() {
    let (mut reg, tx, mut conn, _metrics) = registry_full();
    reg.on_close_conn(kick());
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    let told = read(&mut conn, 3).await;
    assert!(matches!(told[0], ConnIn::LeftRoom { .. }), "{told:?}");
    let named = told
        .iter()
        .filter(|m| matches!(m, ConnIn::ShutdownOvertaking(ServerClose::Kicked)))
        .count();
    let kicks = told.iter().filter(|m| m.verdict().is_some()).count();
    assert_eq!((named, kicks), (1, 1), "{told:?}");
}

/// Refused after the stop, the kick is not counted where it is refused:
/// the connection's end counts what its session lost (F60 — a connection
/// that ended without reading the stop ended on its own).
#[tokio::test]
async fn a_verdict_refused_after_the_stop_is_not_counted_at_the_refusal() {
    let (mut reg, tx, mut conn, metrics) = registry_full();
    reg.on_close_conn(kick());
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    end(&mut conn);
    assert!(lost(metrics).await.is_empty());
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

/// A destroyed room's notice waits the same way: the stop names it
/// under `room_gone`.
#[tokio::test]
async fn the_stop_names_a_room_gone_left_waiting() {
    let (mut reg, tx, mut conn, _metrics) = registry_full();
    reg.notify_room_gone(RoomId(1));
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    let told = read(&mut conn, 3).await;
    let named = told
        .iter()
        .any(|m| matches!(m, ConnIn::ShutdownOvertaking(ServerClose::RoomGone)));
    assert!(named, "{told:?}");
}

/// Two verdicts left waiting: the stop names the first the registry
/// decided — the one the session would have read first had the stop not
/// overtaken them.
#[tokio::test]
async fn the_stop_names_the_first_verdict_left_waiting() {
    let (mut reg, tx, mut conn, _metrics) = registry_full();
    reg.on_close_conn(kick());
    // A second verdict for the (now settled) membership: B43's arm.
    reg.on_close_conn(CloseRequest {
        cause: ServerClose::IdleInput,
        ..kick()
    });
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    let told = read(&mut conn, 4).await;
    let named = told
        .iter()
        .any(|m| matches!(m, ConnIn::ShutdownOvertaking(ServerClose::Kicked)));
    assert!(named, "{told:?}");
}

/// A notice that is no verdict (`LeftRoom`) waiting the same way names
/// nothing: the stop is the plain one.
#[tokio::test]
async fn a_waiting_notice_that_is_no_verdict_names_nothing() {
    let (mut reg, tx, mut conn, _metrics) = registry_full();
    reg.on_leave_conn(LeaveRequest {
        conn: CONN,
        room: RoomId(1),
        entity: 7,
        park: None,
    });
    tx.try_send(RegistryMsg::Shutdown).expect("room");
    reg.run().await;
    let told = read(&mut conn, 3).await;
    assert!(
        told.iter().any(|m| matches!(m, ConnIn::Shutdown)),
        "{told:?}"
    );
}

/// The next `n` messages of the connection's inbox, each in time.
async fn read(conn: &mut Inbox<ConnIn>, n: usize) -> Vec<ConnIn> {
    let mut told = Vec::new();
    for _ in 0..n {
        let msg = tokio::time::timeout(Duration::from_secs(5), conn.recv())
            .await
            .expect("a notice in time")
            .expect("the registry's senders");
        told.push(msg);
    }
    told
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
    let stop = ConnIn::ShutdownOvertaking(ServerClose::Kicked);
    for msg in [frame, client, left, ConnIn::Shutdown, stop] {
        assert_eq!(msg.verdict(), None, "{msg:?}");
    }
}
