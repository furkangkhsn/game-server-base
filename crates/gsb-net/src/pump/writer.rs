//! The writer pump and its progress clock. A child of [`super`], so the
//! stall bookkeeping stays private to the pump module tree.
//!
//! The clock answers ONE question: has anything at all been written
//! successfully to this socket lately? Not "is the client behind" (a
//! client that is merely behind still drains, and the fan-out already
//! tolerates it by dropping snapshots), and not "how old is this frame"
//! (age would punish a low-Hz room and reward a high-Hz one). The
//! symmetric judgment to the rUDP reliable band's liveness bound, which
//! asks the same of the peer's cumulative ACK.
//!
//! Observing it while the pump is parked inside an await is the reader's
//! idiom exactly: wrap the single awaited operation in a deadline. Here
//! the deadline is what REMAINS of the window since the last completed
//! write, so a batch of N frames gets one window in total rather than N.

use std::future::Future;
use std::time::{Duration, Instant};

use futures::SinkExt;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;

/// How one awaited sink operation ended.
enum Step {
    /// It completed: the socket took the bytes, the clock restarts.
    Wrote,
    /// It failed: the peer is definitively gone (the pre-existing exit).
    Gone,
    /// It never completed inside the remaining window: the direction is
    /// dead (see the module docs).
    Stalled,
}

/// Await one sink operation under what is left of the stall window,
/// restarting the window when it completes.
///
/// `tokio::time::timeout` wraps a SINGLE future here, exactly as the
/// reader's idle window does: it is a deadline on one operation, never a
/// second live source — a write that completes always wins.
async fn step<F, E>(fut: F, stall: Option<Duration>, progress: &mut Instant) -> Step
where
    F: Future<Output = Result<(), E>>,
{
    let outcome = match stall {
        Some(window) => {
            let remaining = window.saturating_sub(progress.elapsed());
            if remaining.is_zero() {
                return Step::Stalled;
            }
            match tokio::time::timeout(remaining, fut).await {
                Ok(outcome) => outcome,
                Err(_) => return Step::Stalled,
            }
        }
        None => fut.await,
    };
    match outcome {
        Ok(()) => {
            *progress = Instant::now();
            Step::Wrote
        }
        Err(_) => Step::Gone,
    }
}

/// Spawn the writer pump.
///
/// `in_tx` is the connection actor's mailbox — an IN-PROCESS channel.
/// That is the whole point: when this pump has a verdict to deliver, the
/// socket is the one thing that cannot carry it. The actor then runs its
/// ORDINARY teardown (the `ServerClosed` arm: final metrics flush,
/// `RegistryMsg::ConnClosed`, registry/room release), so the death is
/// indistinguishable from any other end of session.
///
/// `write_stall`: the progress window (`None` disables the clock and the
/// loop is byte-for-byte the pre-existing one).
pub(super) fn spawn<Writer>(
    conn: ConnectionId,
    writer: Writer,
    in_tx: Mailbox<ConnIn>,
    mut out_rx: Inbox<FrameBatch>,
    write_stall: Option<Duration>,
) -> JoinHandle<()>
where
    Writer: futures::Sink<FrameBody, Error = std::io::Error> + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut sink = writer;
        let mut progress = Instant::now();
        let mut stalled: Option<String> = None;
        'batches: while let Some(batch) = out_rx.recv().await {
            // Waiting for work is not a stall: the clock measures a write
            // that cannot finish, never an idle outbound path. (A room
            // between ticks, or a session with nothing to say, must never
            // accumulate window.)
            progress = Instant::now();
            for frame in batch {
                match step(sink.send(frame), write_stall, &mut progress).await {
                    Step::Wrote => {}
                    Step::Gone => {
                        warn!(%conn, "writer pump: send failed; peer gone");
                        break 'batches;
                    }
                    Step::Stalled => {
                        stalled = Some(stall_reason(write_stall));
                        break 'batches;
                    }
                }
            }
            match step(sink.flush(), write_stall, &mut progress).await {
                Step::Wrote => {}
                Step::Gone => {
                    warn!(%conn, "writer pump: flush failed; peer gone");
                    break;
                }
                Step::Stalled => {
                    stalled = Some(stall_reason(write_stall));
                    break;
                }
            }
        }
        // Closing the outbound channel FIRST makes the actor's own next
        // send fail fast (`w_closing`) instead of parking on a channel
        // that is full precisely because this pump stopped draining it.
        drop(out_rx);
        match stalled {
            Some(reason) => {
                warn!(
                    %conn,
                    timeout = ?write_stall,
                    "writer pump: nothing written to the socket for the stall \
                     window; server ending the session"
                );
                // NOT `sink.close()`: a graceful close flushes, and the
                // socket is the thing that is stuck. The teardown must
                // never need the peer to accept one more byte.
                let _ = in_tx.send(ConnIn::ServerClosed { reason }).await;
            }
            // The ordinary exits (peer gone, channel closed) still say
            // goodbye on the wire — under the same window, so a socket
            // that wedges on the way out cannot pin this task forever.
            None => {
                let _ = step(sink.close(), write_stall, &mut progress).await;
            }
        }
        debug!(%conn, "writer pump stopped");
    })
}

/// The reason string carried to the actor (and, from there, to the client
/// as the `ERROR` code 9 message when the wire still works at all).
fn stall_reason(window: Option<Duration>) -> String {
    match window {
        Some(w) => format!("write stall: nothing written to the socket for {w:?}"),
        None => "write stall".into(),
    }
}
