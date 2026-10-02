//! The dispatcher's leave and detach at a room's stop (BACKLOG F55),
//! pinned in both arrival orders. A stopping room closes its control
//! channel (a shard its inbox) in `finish` and counts what it holds
//! (B68: `leaves_unprocessed` / `detaches_unprocessed` for a member still
//! there). An op that arrives FIRST is taken and left to that count; an
//! op that arrives AFTER is refused at the dispatcher and counted
//! nowhere, deliberately: the stop has already ended the member (its
//! session's holdings counted, B62), so the op has nothing left to act
//! on — and the dispatcher could not tell it from an op whose membership
//! the room had ended long before (a destroy, a kick) nor, on a sharded
//! room, which shard owned the member. Either way the dispatcher still
//! settles its side: `LeaveDone` / `DetachDone`, then `OpsClosed`.

use super::*;
use crate::channel::Inbox;
use crate::metrics::MetricsEvent;
use crate::shard::ShardMsg;

/// A dispatcher already seated in `ROOM` (entity `ENTITY`) behind
/// `handle`; its registry reports and its metrics.
fn seated(
    handle: RoomHandle<(), ()>,
) -> (
    tokio::sync::mpsc::Sender<RoomOp<(), ()>>,
    Inbox<RegistryMsg>,
    Inbox<MetricsEvent>,
) {
    let (registry, reports) = channel::<RegistryMsg>(16);
    let (metrics, events) = channel::<MetricsEvent>(8);
    let seed = Some((ROOM, ENTITY, handle, 1, "p".to_string()));
    let ops = Reg::spawn_conn_ops(CONN, registry, metrics, seed, 1);
    (ops, reports, events)
}

/// The dispatcher's close: its `DetachDone` for `ROOM`, then its exit.
async fn detached(reports: &mut Inbox<RegistryMsg>) {
    match reports.recv().await {
        Some(RegistryMsg::DetachDone { conn, room }) => assert_eq!((conn, room), (CONN, ROOM)),
        other => panic!("expected DetachDone, got {other:?}"),
    }
    assert!(matches!(
        reports.recv().await,
        Some(RegistryMsg::OpsClosed { conn: CONN, .. })
    ));
}

/// The room is still taking ops: the detach is queued in its control
/// channel — where the room's stop counts it if it never gets to it —
/// and nothing is sent to the collector.
#[tokio::test]
async fn a_detach_the_room_took_is_left_to_its_stop_count() {
    let (control, mut room) = channel::<RoomControl>(8);
    let (ops, mut reports, mut events) = seated(RoomHandle::Single(control));
    assert!(ops.try_send(RoomOp::Close { verdict: None }).is_ok());
    detached(&mut reports).await;
    assert!(matches!(
        room.try_recv(),
        Ok(RoomControl::Detach {
            conn: CONN,
            entity: ENTITY,
            ..
        })
    ));
    assert!(events.try_recv().is_err(), "the room's stop counts it");
}

/// The room had stopped first (its control channel closed): the detach
/// is refused, the dispatcher still settles, and nothing is counted.
#[tokio::test]
async fn a_detach_the_stopped_room_refused_is_counted_nowhere() {
    let (control, room) = channel::<RoomControl>(8);
    drop(room);
    let (ops, mut reports, mut events) = seated(RoomHandle::Single(control));
    assert!(ops.try_send(RoomOp::Close { verdict: None }).is_ok());
    detached(&mut reports).await;
    assert!(events.try_recv().is_err(), "superseded by the stop");
}

/// A leave, both orders on one sharded room: shard 0 still takes ops,
/// shard 1 has stopped. The broadcast reaches shard 0 (which counts it at
/// its stop only if the member is its own) and is refused by shard 1;
/// the dispatcher reports `LeaveDone` and counts nothing.
#[tokio::test]
async fn a_leave_a_stopped_shard_refused_is_counted_nowhere() {
    let (open, mut shard0) = channel::<ShardMsg<(), ()>>(8);
    let (closed, shard1) = channel::<ShardMsg<(), ()>>(8);
    drop(shard1);
    let (ops, mut reports, mut events) = seated(RoomHandle::Sharded(vec![open, closed]));
    assert!(ops.try_send(RoomOp::Leave { room: ROOM }).is_ok());
    match reports.recv().await {
        Some(RegistryMsg::LeaveDone { conn, room }) => assert_eq!((conn, room), (CONN, ROOM)),
        other => panic!("expected LeaveDone, got {other:?}"),
    }
    assert!(matches!(
        shard0.try_recv(),
        Ok(ShardMsg::Leave {
            conn: CONN,
            entity: ENTITY,
            ..
        })
    ));
    assert!(events.try_recv().is_err(), "nothing to the collector");
}
