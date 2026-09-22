//! The server-close verdict on the connection actor's final sample
//! (`ConnSample::server_close`): every way the actor's session can end,
//! and which of them the actor books as a SERVER verdict.
//!
//! The separator this suite exists for: a counter that only ever reads
//! "some close happened" cannot tell a capacity measurement whether the
//! server shed its clients or the clients left. So every test pins the
//! EXACT reason (a verdict booked under a neighbouring reason fails), and
//! the client-side ends pin `None` (a peer leaving must not read as
//! shedding).
//!
//! The inbox is pre-filled BEFORE the actor runs where ordering matters
//! (the dead-outbound adoption), so nothing here races the actor.

use super::*;

use gsb_core::conn::ServerClose;
use gsb_core::metrics::ConnSample;

/// A connection actor whose inbox is filled by the caller BEFORE it is
/// spawned (`start`): the out channel can be closed up front (the writer
/// pump already gone) and the mailbox order is exactly what was sent.
struct Rig {
    in_tx: mpsc::Sender<ConnIn>,
    out_rx: Option<mpsc::Receiver<FrameBatch>>,
    metrics: mpsc::Receiver<MetricsEvent>,
    actor: Option<ConnectionActor>,
}

impl Rig {
    fn new() -> Self {
        let (in_tx, inbox) = channel::<ConnIn>(128);
        let (out_tx, out_rx) = channel::<FrameBatch>(16);
        let (reg_tx, _reg_rx) = channel::<RegistryMsg>(16);
        let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(64);
        let actor = ConnectionActor::new(
            ConnectionId(77),
            SocketAddr::from(([127, 0, 0, 1], 42_077)),
            Arc::new(test_table()),
            reg_tx,
            inbox,
            out_tx,
            metrics_tx,
            None,
        );
        Self {
            in_tx,
            out_rx: Some(out_rx),
            metrics,
            actor: Some(actor),
        }
    }

    async fn push(&self, msg: ConnIn) {
        self.in_tx.send(msg).await.expect("inbox open");
    }

    /// Run the actor to its exit and return its FINAL sample's verdict.
    async fn verdict(mut self) -> Option<ServerClose> {
        let actor = self.actor.take().expect("started once");
        // Dropping our sender lets `recv` end the loop if nothing else
        // does (the clean client-side end).
        drop(self.in_tx);
        tokio::time::timeout(WAIT, tokio::spawn(actor.run()))
            .await
            .expect("actor did not exit")
            .expect("actor task panicked");
        let mut finals: Vec<ConnSample> = Vec::new();
        while let Ok(ev) = self.metrics.try_recv() {
            if let MetricsEvent::Conn(s) = ev {
                assert!(
                    s.last || s.server_close.is_none(),
                    "only the FINAL sample may carry a verdict: {s:?}"
                );
                if s.last {
                    finals.push(s);
                }
            }
        }
        assert!(finals.len() <= 1, "at most one final sample: {finals:?}");
        finals.first().and_then(|s| s.server_close)
    }
}

fn heartbeat() -> ConnIn {
    ConnIn::Frame(frame(
        op::base::HEARTBEAT,
        &Heartbeat { tick: 1 }.encode_to_vec(),
    ))
}

/// Four hard violations exhaust the budget: booked as
/// `ViolationBudget` — not as the pre-auth budget it sits next to, and
/// not as the dead outbound path its close notice may run into.
#[tokio::test]
async fn the_violation_budget_close_is_booked_as_its_reason() {
    let rig = Rig::new();
    for i in 0..4 {
        rig.push(ConnIn::Frame(frame(42 + i, &[]))).await;
    }
    assert_eq!(rig.verdict().await, Some(ServerClose::ViolationBudget));
}

/// 65 pre-auth frames (heartbeats: ordinary, never violations) cross
/// the §3.3 frame budget: booked as `PreauthBudget`.
#[tokio::test]
async fn the_preauth_budget_close_is_booked_as_its_reason() {
    let rig = Rig::new();
    for _ in 0..65 {
        rig.push(heartbeat()).await;
    }
    assert_eq!(rig.verdict().await, Some(ServerClose::PreauthBudget));
}

/// A pump / registry verdict is booked as the cause it carries — each
/// one, not just one of them.
#[tokio::test]
async fn a_server_closed_notice_is_booked_as_its_cause() {
    for cause in ServerClose::ALL {
        let rig = Rig::new();
        rig.push(ConnIn::ServerClosed {
            cause,
            reason: "test".into(),
        })
        .await;
        assert_eq!(rig.verdict().await, Some(cause), "{cause:?}");
    }
}

/// The transport refusing the stream is the server's verdict; the room
/// dying under the session is too.
#[tokio::test]
async fn stream_rejection_and_room_gone_are_server_verdicts() {
    let rig = Rig::new();
    rig.push(ConnIn::StreamRejected {
        reason: "frame too big".into(),
    })
    .await;
    assert_eq!(rig.verdict().await, Some(ServerClose::StreamRejected));

    let rig = Rig::new();
    rig.push(ConnIn::RoomGone(gsb_core::id::RoomId(1))).await;
    assert_eq!(rig.verdict().await, Some(ServerClose::RoomGone));
}

/// The client-side ends — a peer close, a clean inbox end — and the
/// whole-server shutdown are NOT server verdicts: nothing is booked.
#[tokio::test]
async fn client_side_ends_and_shutdown_book_nothing() {
    let rig = Rig::new();
    rig.push(ConnIn::Closed {
        reason: "peer closed".into(),
    })
    .await;
    assert_eq!(rig.verdict().await, None, "a peer close is not shedding");

    let rig = Rig::new();
    rig.push(heartbeat()).await;
    assert_eq!(rig.verdict().await, None, "a clean end is not shedding");

    let rig = Rig::new();
    rig.push(ConnIn::Shutdown).await;
    assert_eq!(rig.verdict().await, None, "shutdown is not a verdict");
}

/// The write-stall shape exactly: the writer pump posted its verdict and
/// closed the outbound channel, and the actor finds the channel closed
/// while answering a frame queued AHEAD of the verdict. The pending
/// verdict must be adopted — booked as the write stall it is, not as a
/// bare dead outbound path.
#[tokio::test]
async fn a_dead_outbound_path_adopts_the_pending_write_stall() {
    let mut rig = Rig::new();
    drop(rig.out_rx.take()); // the writer pump is gone
    rig.push(heartbeat()).await; // its ACK finds the channel closed
    rig.push(ConnIn::ServerClosed {
        cause: ServerClose::WriteStall,
        reason: "write stall".into(),
    })
    .await;
    assert_eq!(rig.verdict().await, Some(ServerClose::WriteStall));
}

/// The same discovery with the READER's peer close pending behind it:
/// the peer left, so nothing is booked.
#[tokio::test]
async fn a_dead_outbound_path_behind_a_peer_close_books_nothing() {
    let mut rig = Rig::new();
    drop(rig.out_rx.take());
    rig.push(heartbeat()).await;
    rig.push(ConnIn::Closed {
        reason: "connection reset".into(),
    })
    .await;
    assert_eq!(rig.verdict().await, None);
}

/// And with nothing to explain it: `OutboundDead`.
#[tokio::test]
async fn an_unexplained_dead_outbound_path_is_outbound_dead() {
    let mut rig = Rig::new();
    drop(rig.out_rx.take());
    rig.push(heartbeat()).await;
    assert_eq!(rig.verdict().await, Some(ServerClose::OutboundDead));
}
