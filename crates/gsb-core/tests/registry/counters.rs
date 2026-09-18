//! The registry's own sample: the control-plane counters and the two
//! table gauges.
//!
//! `RegistrySample` is the "100k target's scoreboard" (DESIGN §12): room
//! and connection counts, and the flow through them. Every one of these
//! paths is heavily tested through its BEHAVIOUR — the sibling tests in
//! this binary drive joins, leaves, creates and destroys over real
//! frames — but the registry's counters were never read back from a
//! sample. The registry is also the one actor whose counters have no
//! second opinion: a room's numbers can be cross-checked against its
//! fan-out, the registry's cannot.
//!
//! Synchronization: the registry has no timer and flushes a sample on
//! every state change, and it processes its mailbox strictly in order.
//! So the barrier for a message with no reply (`ConnOpened`,
//! `ConnClosed`, `DespawnPlayer`) is a LATER message that does have one
//! (`RoomStatus`) — once its reply is in hand, everything sent before it
//! has been handled and its sample emitted.

use super::*;

// Child module. A `#[path]`-loaded module resolves its OWN children
// against its file's directory, so this path is relative to
// `tests/registry/`.
#[path = "counters/rooms.rs"]
mod rooms;

use gsb_core::metrics::{MetricsEvent, RegistrySample};
use gsb_core::registry::RoomStatus;

/// [`start_registry`] keeping the metrics receiver, so a test can read
/// the registry's own sample instead of inferring it from behaviour.
fn start_observed() -> (
    Mailbox<RegistryMsg>,
    mpsc::Receiver<MetricsEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _ticker_task) = Ticker::spawn(HZ, 64).expect("valid tick rate");
    // Deep enough that no sample in these tests is dropped: a full
    // channel would only bump `metrics_dropped`, but the counters read
    // here come off the LATEST sample, and dropping that one would make
    // the test read a stale state.
    let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(512);
    let handle = tokio::spawn(
        Registry::new(
            rx,
            tx.clone(),
            factory(),
            ticker,
            metrics_tx,
            None,
            None,
            None,
        )
        .run(),
    );
    (tx, metrics_rx, handle)
}

/// Ask for a room's status: a round trip whose reply proves every
/// message sent before it has been processed (the registry handles its
/// mailbox in order). Used as the barrier for the reply-less messages.
async fn settle(tx: &Mailbox<RegistryMsg>) -> RoomStatus {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<RoomStatus>();
    tx.send(RegistryMsg::RoomStatus {
        id: RoomId(9_999),
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
}

/// Drain the metrics channel and return the registry's newest sample, if
/// it emitted one since the last drain. `None` is an ordinary answer: the
/// registry samples on STATE CHANGE, so a round trip that changed nothing
/// (the `settle` barrier itself) produces no new sample.
fn latest_opt(metrics: &mut mpsc::Receiver<MetricsEvent>) -> Option<RegistrySample> {
    let mut last = None;
    while let Ok(ev) = metrics.try_recv() {
        if let MetricsEvent::Registry(s) = ev {
            last = Some(s);
        }
    }
    last
}

/// [`latest_opt`] where the test has just caused a state change, so a
/// sample must exist.
fn latest(metrics: &mut mpsc::Receiver<MetricsEvent>) -> RegistrySample {
    latest_opt(metrics).expect("the registry emitted a sample for this change")
}

async fn close_conn(tx: &Mailbox<RegistryMsg>, conn: ConnectionId) {
    tx.send(RegistryMsg::ConnClosed { conn })
        .await
        .expect("registry gone");
}

async fn despawn(tx: &Mailbox<RegistryMsg>, conn: ConnectionId) {
    tx.send(RegistryMsg::DespawnPlayer { conn })
        .await
        .expect("registry gone");
}

/// `opens` and `closes` are cumulative FLOW; `conns` is the table's
/// current SIZE. Three opens and one close have to read 3 / 1 / 2 — the
/// flow counters keep the history the gauge does not.
///
/// The pairing is the point: an operator diagnosing "the connection cap
/// is refusing joins" needs to know whether the table is full because
/// traffic is high (opens ≈ closes, both large) or because closes are
/// being lost (opens ≫ closes) — a question the gauge alone cannot
/// answer, which is why all three exist.
#[tokio::test]
async fn open_and_close_counters_are_flow_while_conns_is_the_table_size() {
    let (tx, mut metrics, handle) = start_observed();

    for c in 1..=3u64 {
        open_conn(&tx, ConnectionId(c)).await;
    }
    settle(&tx).await;
    let s = latest(&mut metrics);
    assert_eq!(s.opens, 3, "three connections were opened");
    assert_eq!(s.closes, 0);
    assert_eq!(s.conns, 3, "and all three are in the table");

    close_conn(&tx, ConnectionId(2)).await;
    settle(&tx).await;
    let s = latest(&mut metrics);
    assert_eq!(
        s.opens, 3,
        "a close does not undo an open: the flow is cumulative"
    );
    assert_eq!(s.closes, 1, "one connection was closed");
    assert_eq!(s.conns, 2, "the gauge follows the table");

    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not stop")
        .expect("registry task panicked");
}

/// `joins` and `leaves` count the registry's view of the membership
/// funnel — the spawn that COMPLETED and the leave that completed — and
/// they are independent of `opens`/`closes`: a connection can be open
/// without being in any room.
///
/// The third connection here is the separator: it opens and never joins,
/// so a `joins` wired to the open path (or a `leaves` wired to the close
/// path) reads 3 and 0 instead of 2 and 1.
#[tokio::test]
async fn join_and_leave_counters_are_independent_of_open_and_close() {
    let (tx, mut metrics, handle) = start_observed();
    create_room(&tx, RoomId(1)).await;

    let mut outs = Vec::new();
    for c in 1..=2u64 {
        open_conn(&tx, ConnectionId(c)).await;
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        outs.push(out_rx);
        spawn(&tx, ConnectionId(c), RoomId(1), out_tx).await;
    }
    // Opened but never joined: the control on `joins`.
    open_conn(&tx, ConnectionId(3)).await;
    settle(&tx).await;

    let s = latest(&mut metrics);
    assert_eq!(
        s.joins, 2,
        "two spawns completed; the third conn never joined"
    );
    assert_eq!(s.leaves, 0);
    assert_eq!(s.opens, 3, "all three connections were opened");

    despawn(&tx, ConnectionId(1)).await;
    // The leave is reported back by the ROOM (`LeaveDone`), so it lands a
    // tick or two after the despawn: poll until the sample shows it. The
    // last sample seen is carried across iterations — an iteration in
    // which the registry's state did not change emits nothing, which is
    // "not yet", not a failure.
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut s = latest_opt(&mut metrics).unwrap_or(s);
    while s.leaves != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the registry never counted the leave; last sample = {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        settle(&tx).await;
        if let Some(fresh) = latest_opt(&mut metrics) {
            s = fresh;
        }
    }
    assert_eq!(s.joins, 2, "a leave does not undo a join");
    assert_eq!(
        s.closes, 0,
        "leaving a room is not closing a connection: the conn stays registered"
    );

    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not stop")
        .expect("registry task panicked");
}
