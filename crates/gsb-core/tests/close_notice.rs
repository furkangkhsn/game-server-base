//! The close notices of the two server-initiated ends that used to be
//! silent (BACKLOG B12, B13), at the connection actor, no transport:
//!
//! - a server STOP (`ConnIn::Shutdown`) sends `ERROR` code 14
//!   (`ServerStopping`) before the actor ends;
//! - a transport REFUSING the byte stream (`ConnIn::StreamRejected`)
//!   sends `ERROR` code 9 (`ServerClosed`) with the reason, like every
//!   other server verdict.
//!
//! - the room's input-idle ceiling under the opt-in
//!   `afk_action = disconnect` (BACKLOG E6: the registry relays the
//!   room's close request as `ConnIn::ServerClosed { IdleInput }`) sends
//!   `ERROR` code 9 with the reason, like every other server verdict.
//!
//! - a GAME's kick (BACKLOG E8: the room's `ctx.kick`, relayed the same
//!   way as `ConnIn::ServerClosed { Kicked }`) sends `ERROR` code 9 with
//!   the game's reason, under the same rule.
//!
//! All four notices are best effort and never park: the actor enqueues them
//! with a synchronous `try_send`, so a client whose outbound queue is
//! full (it stopped reading) gets only the close — and the actor still
//! ends at once. Without that rule a stop would leave one parked actor
//! per non-draining client, alive until its write-stall window (forever
//! when the window is off).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ConnectionActor, ServerClose};
use gsb_core::id::ConnectionId;
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::RegistryMsg;
use gsb_protocol::base::{Error, ErrorCode};
use gsb_protocol::{FrameBody, base_table, op};
use prost::Message;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(5);
/// The outbound queue's capacity in these tests (small, so filling it
/// is cheap).
const OUT_CAP: usize = 4;

/// A running connection actor, with the test holding a CLONE of its
/// outbound sender (to fill the queue, and so the queue stays open after
/// the actor ends — a full queue must not be mistaken for a dead one).
struct Rig {
    in_tx: mpsc::Sender<ConnIn>,
    out_tx: mpsc::Sender<FrameBatch>,
    out_rx: mpsc::Receiver<FrameBatch>,
    reg_rx: mpsc::Receiver<RegistryMsg>,
    actor: JoinHandle<()>,
}

impl Rig {
    fn start() -> Self {
        let (in_tx, inbox) = channel::<ConnIn>(16);
        let (out_tx, out_rx) = channel::<FrameBatch>(OUT_CAP);
        let (reg_tx, reg_rx) = channel::<RegistryMsg>(16);
        let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(16);
        let actor = ConnectionActor::new(
            ConnectionId(5),
            SocketAddr::from(([127, 0, 0, 1], 43_005)),
            Arc::new(base_table()),
            reg_tx,
            inbox,
            out_tx.clone(),
            metrics_tx,
            None,
        );
        Self {
            in_tx,
            out_tx,
            out_rx,
            reg_rx,
            actor: tokio::spawn(actor.run()),
        }
    }

    /// Fill the outbound queue to the brim: the state a client that
    /// stopped reading leaves behind (its writer pump parked in the
    /// socket write, nothing draining the queue).
    fn fill_outbound(&self) {
        for _ in 0..OUT_CAP {
            self.out_tx
                .try_send(vec![FrameBody::new(op::base::HEARTBEAT_ACK, Vec::new())])
                .expect("queue has room");
        }
    }

    /// Deliver `msg`, then require the actor to END within the bound
    /// and report the connection closed.
    async fn end_with(&mut self, msg: ConnIn) {
        self.in_tx.send(msg).await.expect("inbox open");
        tokio::time::timeout(WAIT, &mut self.actor)
            .await
            .expect("the actor parked instead of ending")
            .expect("no panic");
        let closed = self.reg_rx.try_recv().expect("the registry was told");
        assert!(
            matches!(closed, RegistryMsg::ConnClosed { conn, .. } if conn == ConnectionId(5)),
            "{closed:?}"
        );
    }

    /// Every `ERROR` frame queued for the client, in order.
    fn errors(&mut self) -> Vec<Error> {
        let mut out = Vec::new();
        while let Ok(batch) = self.out_rx.try_recv() {
            for f in batch.iter().filter(|f| f.op == op::base::ERROR) {
                out.push(Error::decode(&f.payload[..]).expect("ERROR decodes"));
            }
        }
        out
    }
}

/// B12: a server stop is announced as ERROR 14, the last frame the
/// connection ever queues.
#[tokio::test]
async fn a_server_stop_sends_the_stopping_notice() {
    let mut rig = Rig::start();
    rig.end_with(ConnIn::Shutdown).await;
    let errors = rig.errors();
    assert_eq!(errors.len(), 1, "exactly one notice: {errors:?}");
    assert_eq!(errors[0].code(), ErrorCode::ServerStopping);
    assert!(!errors[0].message.is_empty(), "the message is always set");
}

/// B13: a refused byte stream is announced as ERROR 9 carrying the
/// transport's reason — the same class as every other server verdict.
#[tokio::test]
async fn a_rejected_stream_sends_the_server_closed_notice() {
    let mut rig = Rig::start();
    rig.end_with(ConnIn::StreamRejected {
        reason: "frame of 9999 bytes exceeds max_frame_bytes".into(),
    })
    .await;
    let errors = rig.errors();
    assert_eq!(errors.len(), 1, "exactly one notice: {errors:?}");
    assert_eq!(errors[0].code(), ErrorCode::ServerClosed);
    assert!(
        errors[0].message.contains("exceeds max_frame_bytes"),
        "the reason reaches the client: {:?}",
        errors[0].message
    );
}

/// THE BOUND: a client that stopped reading cannot hold a stopping
/// server's connection actor. The queue is full, so the notice is
/// dropped — and the actor ends anyway, at once.
#[tokio::test]
async fn a_stop_never_parks_on_a_full_outbound_queue() {
    let mut rig = Rig::start();
    rig.fill_outbound();
    rig.end_with(ConnIn::Shutdown).await;
    assert!(rig.errors().is_empty(), "no room: the notice is dropped");
}

/// The same bound for the stream rejection: a peer that floods bytes the
/// transport refuses while not reading what it is sent must not buy
/// itself a parked actor either.
#[tokio::test]
async fn a_stream_rejection_never_parks_on_a_full_outbound_queue() {
    let mut rig = Rig::start();
    rig.fill_outbound();
    rig.end_with(ConnIn::StreamRejected {
        reason: "websocket protocol violation (1002)".into(),
    })
    .await;
    assert!(rig.errors().is_empty(), "no room: the notice is dropped");
}

/// The idle-input close the room asked for (E6).
fn idle_input_close() -> ConnIn {
    ConnIn::ServerClosed {
        cause: ServerClose::IdleInput,
        reason: "input idle: no game input for 30 s (afk_action = disconnect)".into(),
    }
}

/// E6: the room's idle-input close is announced as ERROR 9 carrying the
/// reason — the server-verdict class — and it is the last frame queued.
#[tokio::test]
async fn an_idle_input_close_sends_the_server_closed_notice() {
    let mut rig = Rig::start();
    rig.end_with(idle_input_close()).await;
    let errors = rig.errors();
    assert_eq!(errors.len(), 1, "exactly one notice: {errors:?}");
    assert_eq!(errors[0].code(), ErrorCode::ServerClosed);
    assert!(
        errors[0].message.contains("input idle"),
        "the reason reaches the client: {:?}",
        errors[0].message
    );
}

/// The bound for E6: the member the ceiling closes is the one most
/// likely to have stopped reading (a backgrounded client), so its notice
/// must not wait on it either — dropped on a full queue, and the actor
/// ends at once (the awaited notice of the older code-9 closes would
/// park it until the write-stall window, forever with the window off).
#[tokio::test]
async fn an_idle_input_close_never_parks_on_a_full_outbound_queue() {
    let mut rig = Rig::start();
    rig.fill_outbound();
    rig.end_with(idle_input_close()).await;
    assert!(rig.errors().is_empty(), "no room: the notice is dropped");
}

/// The kick the room asked for on the game's behalf (E8).
fn kick_close() -> ConnIn {
    ConnIn::ServerClosed {
        cause: ServerClose::Kicked,
        reason: "kicked: speed hack".into(),
    }
}

/// E8: a kick is announced as ERROR 9 carrying the game's reason.
#[tokio::test]
async fn a_kick_close_sends_the_server_closed_notice() {
    let mut rig = Rig::start();
    rig.end_with(kick_close()).await;
    let errors = rig.errors();
    assert_eq!(errors.len(), 1, "exactly one notice: {errors:?}");
    assert_eq!(errors[0].code(), ErrorCode::ServerClosed);
    assert_eq!(errors[0].message, "kicked: speed hack");
}

/// E8's bound: the kicked client may not be reading (the game kicks it
/// for exactly that, often) — the notice never waits on it.
#[tokio::test]
async fn a_kick_close_never_parks_on_a_full_outbound_queue() {
    let mut rig = Rig::start();
    rig.fill_outbound();
    rig.end_with(kick_close()).await;
    assert!(rig.errors().is_empty(), "no room: the notice is dropped");
}
