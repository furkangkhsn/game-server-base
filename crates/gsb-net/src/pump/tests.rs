//! The pumps and the connection actor together: the write-stall verdict
//! must reach the actor's final metrics sample as `write_stall` even
//! when the actor's mailbox is FULL at the moment of the verdict.
//!
//! That is the overload shape (the 10k measurement booked 67-172 closes
//! per run as `outbound_dead`): a client that stops reading but keeps
//! sending. The actor answers one of its frames, parks on the outbound
//! channel the wedged writer stopped draining, and the reader keeps
//! filling the mailbox behind it. When the stall window runs out the
//! writer closes the outbound channel; the actor's parked send fails
//! (`w_closing`) and it looks in its mailbox for the reason
//! (`adopt_pending_close`). The verdict has to be there already — a
//! verdict that waits for a free mailbox slot arrives after the actor
//! has looked and left.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Sink;
use tokio::sync::mpsc;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ConnectionActor, ServerClose};
use gsb_core::id::ConnectionId;
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::RegistryMsg;
use gsb_protocol::{FrameBody, base, base_table, op};

use super::{PumpTimeouts, WriteProgress, spawn_pumps};

/// A socket that takes a frame and then never accepts a byte: every
/// flush and close stays pending, and the byte count never moves.
struct Wedged;

impl Sink<FrameBody> for Wedged {
    type Error = std::io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, _item: FrameBody) -> std::io::Result<()> {
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}

impl WriteProgress for Wedged {
    fn bytes_written(&self) -> u64 {
        0
    }
}

/// THE PROPERTY: a stall verdict delivered into a full mailbox is still
/// the reason the session is booked under.
///
/// Single-threaded runtime on purpose: the actor's look into its mailbox
/// is synchronous, so a verdict still waiting for a slot cannot slip in
/// while the actor looks — without the fix this fails every run, booked
/// as `OutboundDead`.
#[tokio::test]
async fn a_stall_verdict_into_a_full_mailbox_is_booked_as_write_stall() {
    let table = Arc::new(base_table());
    let (in_tx, inbox) = channel::<ConnIn>(4);
    let (out_tx, out_rx) = channel::<FrameBatch>(1);
    let (reg_tx, _reg_rx) = channel::<RegistryMsg>(16);
    let (metrics_tx, mut metrics_rx) = mpsc::channel::<MetricsEvent>(64);
    let actor = ConnectionActor::new(
        ConnectionId(21),
        SocketAddr::from(([127, 0, 0, 1], 41_021)),
        table.clone(),
        reg_tx,
        inbox,
        out_tx,
        metrics_tx,
        None,
    );
    let actor = tokio::spawn(actor.run());
    let (read, write) = spawn_pumps(
        ConnectionId(21),
        futures::stream::pending::<std::io::Result<FrameBody>>(),
        Wedged,
        in_tx.clone(),
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(Duration::from_millis(300)),
        },
        None,
    );

    // AUTH: its result is the frame the writer takes and wedges on.
    let auth = base::Auth {
        name: "stalled".into(),
        ticket: vec![],
        protocol_version: 0,
    };
    let auth = table.frame(op::base::AUTH_REQ, &auth).expect("AUTH_REQ");
    in_tx.send(ConnIn::Frame(auth)).await.expect("inbox open");
    // Two answered frames (not in a room: a race-class ERROR each, well
    // under the budget): the first fills the one-slot outbound channel,
    // the second parks the actor on it.
    for _ in 0..2 {
        let leave = FrameBody::new(op::base::LEAVE_ROOM_REQ, Vec::new());
        in_tx.send(ConnIn::Frame(leave)).await.expect("inbox open");
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The client keeps sending: fill the mailbox the parked actor no
    // longer drains, and keep it full through the stall verdict.
    let hb = FrameBody::new(op::base::HEARTBEAT, Vec::new());
    let mut queued = 0;
    while in_tx.try_send(ConnIn::Frame(hb.clone())).is_ok() {
        queued += 1;
    }
    assert!(queued > 0, "the mailbox took the client's frames");

    let close = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match metrics_rx.recv().await.expect("the actor flushes") {
                MetricsEvent::Conn(s) if s.last => break s.server_close,
                _ => {}
            }
        }
    })
    .await
    .expect("the stalled session ended");
    assert_eq!(
        close,
        Some(ServerClose::WriteStall),
        "a full mailbox must not turn the write stall into a bare dead outbound path"
    );

    tokio::time::timeout(Duration::from_secs(5), actor)
        .await
        .expect("the actor exited")
        .expect("no panic");
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the writer pump exited without the wedged socket")
        .expect("no panic");
    read.abort();
}

/// What the pumps lose at their end, counted (B66). A CHILD module: it
/// reuses the wedged socket above.
mod lost;

/// The idle window against a stalled process (F72). A CHILD module: it
/// reuses the wedged socket above.
mod idle;
