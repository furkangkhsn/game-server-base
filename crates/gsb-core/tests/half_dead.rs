//! Half-dead connection teardown: a session whose OUTBOUND path is
//! definitively gone must not keep holding its slot.
//!
//! The reader pump's idle window (`idle_timeout_secs`) covers the SILENT
//! peer: no frames arrive, the window fires, the teardown cascade runs.
//! It says nothing about the other half of the socket. The writer pump
//! exits when a socket write or flush fails, and it holds no inbox — so
//! its exit is silent. The only remaining evidence is that the outbound
//! channel is now CLOSED, and all three holders of that channel used to
//! ignore it: the room's fan-out treats `Closed` exactly like `Full`
//! (drop the batch, retry next tick, forever) and the connection actor
//! discarded its send result outright.
//!
//! The result was a session that could never receive another byte yet
//! kept its registry row and its room slot — until the reader's idle
//! window happened to notice, and forever when `idle_timeout_secs = 0`
//! disables that window.
//!
//! The actor is driven here directly over its inbox — no transport.
//! Dropping the out receiver is exactly the state the writer pump leaves
//! behind when the socket write fails, and a live registry receiver makes
//! the teardown observable as `RegistryMsg::ConnClosed`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::ConnectionId;
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::RegistryMsg;
use gsb_protocol::base::{Auth, Heartbeat};
use gsb_protocol::{base_table, op};
use prost::Message;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(5);

fn frame(op: u16, payload: &[u8]) -> gsb_protocol::FrameBody {
    gsb_protocol::FrameBody::new(op, payload.to_vec())
}

/// Spawn a connection actor with a LIVE registry receiver (so the
/// teardown cascade's first hop is observable) and the out channel in
/// hand (so the test controls when the writer "dies").
#[allow(clippy::type_complexity)]
fn spawn_actor(
    conn: u64,
) -> (
    mpsc::Sender<ConnIn>,
    mpsc::Receiver<FrameBatch>,
    mpsc::Receiver<RegistryMsg>,
    JoinHandle<()>,
) {
    let (inbox_tx, inbox) = channel::<ConnIn>(128);
    let (out_tx, out_rx) = channel::<FrameBatch>(16);
    let (reg_tx, reg_rx) = channel::<RegistryMsg>(16);
    let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(16);
    let actor = ConnectionActor::new(
        ConnectionId(conn),
        SocketAddr::from(([127, 0, 0, 1], 41_000u16 + conn as u16)),
        Arc::new(base_table()),
        reg_tx,
        inbox,
        out_tx,
        metrics_tx,
        None,
    );
    let h = tokio::spawn(actor.run());
    (inbox_tx, out_rx, reg_rx, h)
}

/// Authenticate the connection and consume its `AUTH_RESULT`, so the
/// session is in the post-auth state where a heartbeat is answered
/// unconditionally (the pre-auth 1/s ACK throttle no longer applies).
async fn authenticate(in_tx: &mpsc::Sender<ConnIn>, out: &mut mpsc::Receiver<FrameBatch>) {
    let auth = Auth {
        name: "halfdead".into(),
        ticket: vec![],
        protocol_version: 0,
    }
    .encode_to_vec();
    in_tx
        .send(ConnIn::Frame(frame(op::base::AUTH_REQ, &auth)))
        .await
        .expect("inbox open");
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out waiting for AUTH_RESULT")
        .expect("out open");
    assert!(
        batch.iter().any(|f| f.op == op::base::AUTH_RESULT),
        "the connection authenticated"
    );
}

/// THE PROPERTY: once the outbound channel is closed — the writer pump's
/// only trace after a failed socket write — the next frame the actor
/// handles tears the session down, releasing the registry row and the
/// room slot. No idle window is involved, and none is required.
#[tokio::test]
async fn closed_out_channel_tears_the_session_down() {
    let (in_tx, mut out, mut reg, handle) = spawn_actor(1);
    authenticate(&in_tx, &mut out).await;

    // Drain the registry hop the AUTH produced, so the assertion below
    // cannot satisfy itself from an earlier message.
    while reg.try_recv().is_ok() {}

    // The writer pump dies: its socket write failed and it dropped the
    // receiver. Nothing notifies anyone — that is the whole point.
    drop(out);

    // A live client keeps heartbeating (this is precisely the case the
    // idle window can never catch: traffic IS arriving). Post-auth the
    // actor answers every heartbeat, so it discovers the dead channel on
    // the very next one.
    let hb = Heartbeat { tick: 1 }.encode_to_vec();
    in_tx
        .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
        .await
        .expect("inbox open");

    let msg = tokio::time::timeout(WAIT, reg.recv())
        .await
        .expect(
            "the actor never reported the unreachable session: it is still \
             holding its registry row and room slot",
        )
        .expect("registry channel open");
    assert!(
        matches!(msg, RegistryMsg::ConnClosed { conn } if conn == ConnectionId(1)),
        "the teardown cascade's first hop is ConnClosed for this connection, got {msg:?}"
    );

    tokio::time::timeout(WAIT, handle)
        .await
        .expect("the actor task exited")
        .expect("no panic");
}

/// The converse, so the fix cannot be "close on every write": a healthy
/// connection whose out channel is alive keeps running across many
/// heartbeats and stays reachable. This is the regression guard for
/// `active_heartbeat_survives` at the actor level — liveness traffic must
/// not become a reason to disconnect.
///
/// Note the heartbeats here arrive back to back, far above the §3.2
/// answer rate, so only the first and the one after the interval are
/// ANSWERED; the rest are counted and silently ignored. That is the
/// throttle, not a teardown — which is exactly what this test has to
/// tell apart.
#[tokio::test]
async fn healthy_out_channel_keeps_the_session_alive() {
    let (in_tx, mut out, mut reg, handle) = spawn_actor(2);
    authenticate(&in_tx, &mut out).await;
    while reg.try_recv().is_ok() {}

    for tick in 0..8u64 {
        let hb = Heartbeat { tick }.encode_to_vec();
        in_tx
            .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
            .await
            .expect("inbox open");
        if tick == 0 {
            let batch = tokio::time::timeout(WAIT, out.recv())
                .await
                .expect("timed out waiting for HEARTBEAT_ACK")
                .expect("out open");
            assert!(
                batch.iter().any(|f| f.op == op::base::HEARTBEAT_ACK),
                "the first post-auth heartbeat is answered"
            );
        }
    }
    assert!(
        reg.try_recv().is_err(),
        "a reachable connection is never reported closed"
    );
    // Still reachable after the throttle interval: an answer, not a
    // close — the session survived the whole burst.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let hb = Heartbeat { tick: 99 }.encode_to_vec();
    in_tx
        .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
        .await
        .expect("inbox open");
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out waiting for the post-interval HEARTBEAT_ACK")
        .expect("out open");
    assert!(
        batch.iter().any(|f| f.op == op::base::HEARTBEAT_ACK),
        "the session is alive and answering again after the interval"
    );
    assert!(
        reg.try_recv().is_err(),
        "a reachable connection is never reported closed"
    );

    // It still ends on the ordinary path.
    in_tx.send(ConnIn::Shutdown).await.expect("inbox open");
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("the actor task exited")
        .expect("no panic");
}
