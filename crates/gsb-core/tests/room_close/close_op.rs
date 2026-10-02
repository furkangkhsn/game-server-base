//! A close op the connection's dispatcher never received (BACKLOG B61).
//! The dispatcher's bounded op queue is full when the connection closes,
//! so the registry's `try_send(RoomOp::Close)` is refused (and counted,
//! `close_ops_dropped`). The joins queued ahead of the close still run —
//! each re-join supersedes the last, so the connection ends up in the
//! room as a NEW entity the registry's table has not seen yet. That last
//! membership must still end the way every membership of a closing
//! connection ends: the game's `on_disconnect` once, as a closed
//! connection (F27), then the despawn's report releases the row and the
//! member slot. Before the fix the dispatcher drained its queue and
//! exited without the detach: the member, its row and its slot stayed
//! until the room itself ended, and `on_disconnect` never ran.
//!
//! Filling the queue is deterministic (the `registry::counters::ops`
//! idiom): this current-thread runtime runs the registry through the
//! whole burst — and the close behind it — before the dispatcher runs
//! again, so the 16-deep queue takes the first 16 joins and refuses the
//! rest.

use super::*;
use gsb_core::registry::Seat;
use rejoin_rig::{Hook, factory_with, table_size};

/// Joins queued behind the settled one: more than the queue holds.
const BURST: u64 = 40;
/// The dispatcher's op queue depth.
const QUEUE: u64 = 16;

type Reply = oneshot::Receiver<Result<Seat, CoreError>>;
type Receiver = mpsc::Receiver<FrameBatch>;

/// A raw anonymous join of room 1, its reply left to the caller.
async fn join(tx: &Mailbox<RegistryMsg>, conn: ConnectionId, outs: &mut Vec<Receiver>) -> Reply {
    let (out, out_rx) = channel::<FrameBatch>(4096);
    outs.push(out_rx);
    let (reply, rx) = oneshot::channel();
    tx.send(RegistryMsg::SpawnPlayer {
        conn,
        room: RoomId(1),
        out,
        identity: String::new(),
        reply,
        claims: None,
    })
    .await
    .expect("registry alive");
    rx
}

async fn open(tx: &Mailbox<RegistryMsg>, conn: ConnectionId) -> mpsc::Receiver<ConnIn> {
    let (inbox, rx) = channel::<ConnIn>(64);
    tx.send(RegistryMsg::ConnOpened {
        conn,
        inbox,
        source: None,
    })
    .await
    .expect("registry alive");
    rx
}

/// The registry's newest sample so far.
fn close_ops_dropped(metrics: &mut mpsc::Receiver<MetricsEvent>) -> Option<u64> {
    let mut last = None;
    while let Ok(ev) = metrics.try_recv() {
        if let MetricsEvent::Registry(s) = ev {
            last = Some(s.close_ops_dropped);
        }
    }
    last
}

async fn refused_close_still_ends_the_membership(sharded: bool) {
    let (hooks_tx, mut hooks) = mpsc::unbounded_channel();
    let (causes_tx, mut causes) = mpsc::unbounded_channel();
    let (tx, mut metrics) = start(factory_with(sharded, false, hooks_tx, Some(causes_tx)));
    let config = RoomConfig {
        id: RoomId(1),
        max_players: Some(1),
        ..Default::default()
    };
    create(&tx, config).await;
    let conn = ConnectionId(1);
    let _inbox = open(&tx, conn).await;
    let mut outs = Vec::new();

    // One settled membership: the row holds room 1.
    let first = join(&tx, conn, &mut outs).await;
    tokio::time::timeout(WAIT, first)
        .await
        .expect("in time")
        .expect("answered")
        .expect("joined");
    let deadline = tokio::time::Instant::now() + WAIT;
    while status(&tx, RoomId(1)).await != (RoomStatus::Running { members: 1 }) {
        assert!(tokio::time::Instant::now() < deadline, "the join settles");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The burst and the close behind it, with no yield in between.
    let mut replies = Vec::new();
    for _ in 0..BURST {
        replies.push(join(&tx, conn, &mut outs).await);
    }
    tx.send(RegistryMsg::ConnClosed {
        conn,
        verdict: None,
    })
    .await
    .expect("registry alive");
    let mut refused = 0;
    for r in replies {
        let answer = tokio::time::timeout(WAIT, r).await.expect("in time");
        if answer.is_err() {
            refused += 1;
        }
    }
    assert_eq!(refused, BURST - QUEUE, "the queue took {QUEUE} joins");
    assert_eq!(
        close_ops_dropped(&mut metrics),
        Some(1),
        "the close op was refused (the scenario under test)"
    );

    // The last drained join's membership ends once, as a closed connection.
    // (The logic mints player = entity = its join count: the settled
    // join, then the queue's.)
    let last = PlayerId(1 + QUEUE);
    let ended = tokio::time::timeout(WAIT, causes.recv())
        .await
        .expect("on_disconnect never ran: the refused close lost the detach")
        .expect("logic alive");
    assert_eq!(ended, (last, DisconnectCause::ConnectionClosed));

    // No member, no row, and the one slot takes a new player.
    let deadline = tokio::time::Instant::now() + WAIT;
    while status(&tx, RoomId(1)).await != (RoomStatus::Running { members: 0 }) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the closed connection's row still holds room 1"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(table_size(&tx, &mut metrics).await, 0, "no row left");
    let _second = open(&tx, ConnectionId(2)).await;
    let next = join(&tx, ConnectionId(2), &mut outs).await;
    tokio::time::timeout(WAIT, next)
        .await
        .expect("in time")
        .expect("answered")
        .expect("the slot came back (sharded={sharded})");

    let log: Vec<Hook> = std::iter::from_fn(|| hooks.try_recv().ok()).collect();
    let disconnects: Vec<&Hook> = log
        .iter()
        .filter(|h| matches!(h, Hook::Disconnect(_)))
        .collect();
    assert_eq!(disconnects, vec![&Hook::Disconnect(last)], "once: {log:?}");
    assert!(causes.try_recv().is_err(), "one cause, once");
}

#[tokio::test]
async fn a_refused_close_op_still_detaches_the_member() {
    refused_close_still_ends_the_membership(false).await;
}

#[tokio::test]
async fn a_refused_close_op_still_frees_the_grid_member_slot() {
    refused_close_still_ends_the_membership(true).await;
}
