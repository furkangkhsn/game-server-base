//! The dispatcher task counts a join the room refused (BACKLOG B75): the
//! client's answer is the `RoomGone` it always was, the registry still
//! hears `SpawnFailed` (its reservation goes), and the collector gets one
//! `JoinRefusedClosed` — straight from the dispatcher, which outlives the
//! registry at the whole-server stop. A join the room took and dropped
//! is the room's stop count, never this one.

use super::*;
use crate::error::CoreError;
use crate::metrics::MetricsEvent;
use crate::registry::Seat;

/// One join through a fresh dispatcher into `handle`; returns the
/// client's answer receiver, the registry's reports and the metrics.
fn join_through(
    handle: RoomHandle<(), ()>,
) -> (
    oneshot::Receiver<Result<Seat, CoreError>>,
    crate::channel::Inbox<RegistryMsg>,
    crate::channel::Inbox<MetricsEvent>,
) {
    let (registry, reports) = channel::<RegistryMsg>(16);
    let (metrics, events) = channel::<MetricsEvent>(8);
    let ops = Reg::spawn_conn_ops(CONN, registry, metrics, None, 1);
    let (out, _out) = channel(8);
    let (reply, answer) = oneshot::channel();
    let join = RoomOp::Join {
        room: ROOM,
        handle,
        shard: None,
        generation: 0,
        epoch: 1,
        out,
        identity: String::new(),
        input_rate: None,
        reply,
        claims: None,
    };
    assert!(ops.try_send(join).is_ok());
    (answer, reports, events)
}

/// The room had stopped (its control inbox closed) before the join got
/// there: `RoomGone` to the client, `SpawnFailed` to the registry, one
/// refusal to the collector.
#[tokio::test]
async fn a_join_a_stopped_room_refuses_is_counted_once() {
    let (control, room) = channel::<RoomControl>(8);
    drop(room);
    let (answer, mut reports, mut events) = join_through(RoomHandle::Single(control));
    assert!(matches!(
        answer.await.expect("answered"),
        Err(CoreError::RoomGone)
    ));
    assert!(matches!(
        reports.recv().await,
        Some(RegistryMsg::SpawnFailed { conn: CONN, .. })
    ));
    assert!(matches!(
        events.try_recv(),
        Ok(MetricsEvent::JoinRefusedClosed)
    ));
    assert!(events.try_recv().is_err(), "counted once");
}

/// The room took the join and stopped with it queued (its stop counts
/// `joins_unprocessed`): the same answer, and nothing to the collector.
#[tokio::test]
async fn a_join_the_room_took_and_dropped_is_not_counted_here() {
    let (control, mut room) = channel::<RoomControl>(8);
    let (answer, mut reports, mut events) = join_through(RoomHandle::Single(control));
    match room.recv().await {
        Some(RoomControl::Join { .. }) => {}
        other => panic!("expected the join, got {other:?}"),
    }
    assert!(matches!(
        answer.await.expect("answered"),
        Err(CoreError::RoomGone)
    ));
    assert!(matches!(
        reports.recv().await,
        Some(RegistryMsg::SpawnFailed { conn: CONN, .. })
    ));
    assert!(events.try_recv().is_err(), "the room's stop counts it");
}
