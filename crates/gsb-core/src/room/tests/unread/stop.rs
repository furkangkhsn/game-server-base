//! What a stopping room still holds (BACKLOG B62): the stop ends every
//! session, and what they take along is counted as a session end counts
//! it — unread input (a live row's channel and a parked row's dead one),
//! owed answers, requests in flight — then handed to the collector in
//! the room's final sample. Before B62 a stopping room sent nothing more.

use std::collections::VecDeque;
use std::time::Instant;

use super::*;
use crate::rpc::{PendingRequest, RpcReply};

fn owed(id: u64) -> RpcReply {
    RpcReply {
        id,
        ok: true,
        op: 1,
        reason: String::new(),
        payload: bytes::Bytes::new(),
    }
}

/// A live row with two requests and an action unread, a parked row whose
/// dead channel holds the same, one answer owed and two requests in
/// flight: the stop counts each, and the final sample — the last event —
/// carries them, with no request left pending.
#[tokio::test]
async fn a_stopping_room_counts_what_it_holds_in_its_final_sample() {
    let mut r = room(Detach::Hold {
        grace: None,
        to: ExpireTo::Despawn,
    });
    let (metrics, mut samples) = mpsc::channel(8);
    r.metrics = metrics;
    let parked = join(&mut r, 1, "p");
    send_two_requests(&parked, 1);
    drop(parked);
    detach(&mut r, 1);
    let live = join(&mut r, 2, "");
    send_two_requests(&live, 2);
    r.queued.insert(ConnectionId(2), vec![owed(7)]);
    let due = Instant::now();
    r.pending.insert(
        ConnectionId(2),
        VecDeque::from([
            PendingRequest { id: 8, op: 1, due },
            PendingRequest { id: 9, op: 1, due },
        ]),
    );
    r.pending_total = 2;

    r.finish();

    let mut last = None;
    while let Ok(ev) = samples.try_recv() {
        last = Some(ev);
    }
    let Some(MetricsEvent::RoomFinal(s)) = last else {
        panic!("the final sample is the room's last event: {last:?}");
    };
    assert_eq!(s.room, RoomId(36));
    assert_eq!(s.requests_dropped_unread, 4, "two per row, parked included");
    assert_eq!(s.actions_dropped_unread, 2, "one per row, apart");
    assert_eq!(s.requests_undelivered, 1, "the owed answer");
    assert_eq!(s.requests_abandoned, 2, "the two in flight");
    assert_eq!(s.pending_requests, 0, "nothing is pending after the stop");
    assert!(live.is_closed(), "the live row's channel is closed");
}

/// The final sample goes out past a full metrics channel: from a
/// spawned sender, once the collector reads.
#[tokio::test]
async fn the_final_sample_is_not_lost_to_a_full_channel() {
    let mut r = room(Detach::Despawn);
    let (metrics, mut samples) = mpsc::channel(1);
    metrics
        .try_send(MetricsEvent::RoomGone(RoomId(0)))
        .expect("the one slot was free");
    r.metrics = metrics;
    r.finish();
    let wait = Duration::from_secs(5);
    assert!(matches!(
        tokio::time::timeout(wait, samples.recv()).await,
        Ok(Some(MetricsEvent::RoomGone(_)))
    ));
    assert!(
        matches!(
            tokio::time::timeout(wait, samples.recv()).await,
            Ok(Some(MetricsEvent::RoomFinal(_)))
        ),
        "the final sample, delivered once the slot freed"
    );
}

/// The control channel's leftovers at the stop (B68): a join and a
/// resume never admitted, a leave and a detach for a member here, each
/// counted once; a stale leave (no such member) and a detach for an
/// already parked row lose nothing and are not counted. The channel is
/// closed: a later op is refused at its sender.
#[tokio::test]
async fn a_stopping_room_counts_the_ops_left_in_its_control_channel() {
    let mut r = room(Detach::Hold {
        grace: None,
        to: ExpireTo::Despawn,
    });
    let (metrics, mut samples) = mpsc::channel(8);
    r.metrics = metrics;
    let _live = join(&mut r, 2, "");
    let _parked = join(&mut r, 1, "p");
    detach(&mut r, 1);
    let (ctl, control_rx) = channel(16);
    r.control_rx = control_rx;
    let (out, _out_rx) = mpsc::channel::<FrameBatch>(8);
    let (reply, joined) = oneshot::channel();
    let ops = [
        RoomControl::Join {
            conn: ConnectionId(5),
            out: out.clone(),
            reply,
        },
        RoomControl::Resume {
            conn: ConnectionId(6),
            epoch: 0,
            identity: "q".to_string(),
            out,
            reply: oneshot::channel().0,
        },
        RoomControl::Leave {
            conn: ConnectionId(2),
            entity: 2,
        },
        RoomControl::Leave {
            conn: ConnectionId(9),
            entity: 9,
        },
        RoomControl::Detach {
            conn: ConnectionId(2),
            entity: 2,
            identity: String::new(),
        },
        RoomControl::Detach {
            conn: ConnectionId(1),
            entity: 1,
            identity: "p".to_string(),
        },
        RoomControl::Shutdown,
    ];
    for op in ops {
        ctl.try_send(op).expect("room in the channel");
    }

    r.finish();

    let mut last = None;
    while let Ok(ev) = samples.try_recv() {
        last = Some(ev);
    }
    let Some(MetricsEvent::RoomFinal(s)) = last else {
        panic!("the final sample: {last:?}");
    };
    assert_eq!(s.stop.joins_unprocessed, 1);
    assert_eq!(s.stop.resumes_unprocessed, 1);
    assert_eq!(
        s.stop.leaves_unprocessed, 1,
        "the stale leave is not a loss"
    );
    assert_eq!(
        s.stop.detaches_unprocessed, 1,
        "the parked row's is a duplicate"
    );
    assert_eq!(s.stop.migrations_in_dropped, 0, "a room has no migrations");
    assert!(joined.await.is_err(), "the join's reply was dropped");
    assert!(
        ctl.try_send(RoomControl::Shutdown).is_err(),
        "the channel is closed"
    );
}
