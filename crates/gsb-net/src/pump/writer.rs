//! The writer pump and its progress clock. A child of [`super`], so the
//! stall bookkeeping stays private to the pump module tree.
//!
//! The clock answers ONE question: has the socket accepted any BYTE
//! lately? Not "is the client behind" (a client that is merely behind
//! still drains, and the fan-out already tolerates it by dropping
//! snapshots), not "how old is this frame" (age would punish a low-Hz
//! room and reward a high-Hz one), and not "did the last frame finish"
//! (a frame that takes longer than the window to drain would kill a peer
//! that reads all along — and as frames grow, a completion clock quietly
//! turns into an age bound). The symmetric judgment to the rUDP reliable
//! band's liveness bound, which asks the same of the peer's cumulative
//! ACK.
//!
//! Observing it while the pump is parked inside an await is the reader's
//! idiom: wrap the single awaited operation in a deadline. The deadline
//! is what REMAINS of the window since the transport last accepted a byte
//! ([`WriteProgress`], read through the pending operation itself — see
//! [`op`]); when it fires with bytes having moved in the meantime, the
//! window restarts and the SAME pending operation is awaited again (a
//! half-written frame is never dropped or re-sent). A batch of N frames
//! still gets one window in total, not N: only bytes restart it.

mod op;

use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tracing::{debug, warn};

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::{ConnIn, ServerClose};
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;

use crate::pump::WriteProgress;
use crate::pump::lost::Unwritten;
use crate::pump::verdict::Verdict;
use op::Op;

/// How one awaited sink operation ended.
enum Step {
    /// It completed: the socket took the bytes, the clock restarts.
    Wrote,
    /// It failed: the peer is definitively gone (the pre-existing exit).
    Gone,
    /// The transport accepted no byte for a whole window while it was
    /// pending: the direction is dead (see the module docs).
    Stalled,
}

/// Await one sink operation under the stall window, restarting the
/// window whenever the transport accepts a byte, and once more when the
/// operation completes.
///
/// `tokio::time::timeout` wraps a SINGLE future here, exactly as the
/// reader's idle window does: it is a deadline on one operation, never a
/// second live source — a write that completes always wins. What is new
/// is only what happens when the deadline fires: the operation is not
/// dropped (it is borrowed, `&mut op`, not moved into the timeout), the
/// byte count is read, and if it moved the same operation is awaited
/// under a fresh window.
async fn step<W>(mut op: Op<'_, W>, stall: Option<Duration>, progress: &mut Instant) -> Step
where
    W: futures::Sink<FrameBody, Error = std::io::Error> + WriteProgress + Unpin,
{
    let outcome = match stall {
        Some(window) => loop {
            let remaining = window.saturating_sub(op.observe().elapsed());
            if remaining.is_zero() {
                return Step::Stalled;
            }
            match tokio::time::timeout(remaining, &mut op).await {
                Ok(outcome) => break outcome,
                // Pending at the deadline. Loop: `observe` above re-reads
                // the byte count — bytes accepted since the last look
                // (on a door whose socket is written by another task,
                // this is where they are first seen) restart the window.
                Err(_) => continue,
            }
        },
        None => op.await,
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
/// socket is the one thing that cannot carry it. One slot of it is
/// reserved here, before the task starts ([`crate::pump::verdict`]): the verdict must
/// land even when the mailbox is full. The actor then runs its
/// ORDINARY teardown (the `ServerClosed` arm: final metrics flush,
/// `RegistryMsg::ConnClosed`, registry/room release), so the death is
/// indistinguishable from any other end of session.
///
/// `write_stall`: the progress window (`None` disables the clock: every
/// operation is simply awaited to completion). `metrics`: where the
/// frames it never writes are counted (B66, [`Unwritten`]).
pub(super) fn spawn<Writer>(
    conn: ConnectionId,
    writer: Writer,
    in_tx: Mailbox<ConnIn>,
    mut out_rx: Inbox<FrameBatch>,
    write_stall: Option<Duration>,
    metrics: crate::TransportMetrics,
) -> JoinHandle<()>
where
    Writer:
        futures::Sink<FrameBody, Error = std::io::Error> + WriteProgress + Unpin + Send + 'static,
{
    let verdict = Verdict::reserve(in_tx, write_stall.is_some());
    tokio::spawn(async move {
        let mut sink = writer;
        let mut progress = Instant::now();
        let mut stalled: Option<String> = None;
        let mut unwritten = Unwritten::default();
        'batches: while let Some(batch) = out_rx.recv().await {
            // Waiting for work is not a stall: the clock measures a write
            // that cannot finish, never an idle outbound path. (A room
            // between ticks, or a session with nothing to say, must never
            // accumulate window.)
            progress = Instant::now();
            let len = batch.len();
            for (i, frame) in batch.into_iter().enumerate() {
                match step(
                    Op::send(&mut sink, frame, progress),
                    write_stall,
                    &mut progress,
                )
                .await
                {
                    Step::Wrote => {}
                    Step::Gone => {
                        warn!(%conn, "writer pump: send failed; peer gone");
                        unwritten.rest_of_batch(len - i);
                        break 'batches;
                    }
                    Step::Stalled => {
                        stalled = Some(stall_reason(write_stall));
                        unwritten.rest_of_batch(len - i);
                        break 'batches;
                    }
                }
            }
            match step(Op::flush(&mut sink, progress), write_stall, &mut progress).await {
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
        match stalled {
            Some(reason) => {
                warn!(
                    %conn,
                    timeout = ?write_stall,
                    "writer pump: nothing written to the socket for the stall \
                     window; server ending the session"
                );
                // The verdict is POSTED before the outbound channel is
                // closed, into the slot reserved at birth — synchronous,
                // and it cannot fail on a full mailbox: the actor is
                // typically parked on that full channel right now, and the
                // close below wakes it with a failed send (`w_closing`) —
                // at which point it looks in its mailbox for the reason
                // (`adopt_pending_close`). Posting first means the reason
                // is already there to find, so the close is counted as the
                // write stall it is rather than as a bare dead outbound
                // path — overload included, where the mailbox is full of
                // the client's frames (see [`crate::pump::verdict`]).
                let deferred = verdict.post(ConnIn::ServerClosed {
                    cause: ServerClose::WriteStall,
                    reason,
                });
                // Closing the outbound channel makes the actor's own next
                // send fail fast instead of parking on a channel that is
                // full precisely because this pump stopped draining it.
                // What it still holds is counted (B66).
                unwritten.drain(&mut out_rx);
                drop(out_rx);
                // NOT `sink.close()`: a graceful close flushes, and the
                // socket is the thing that is stuck. The teardown must
                // never need the peer to accept one more byte.
                // Only when no slot could be reserved at birth: the
                // pre-existing order (the verdict may miss the actor).
                // Counted (B66): the close may be booked as
                // `outbound_dead` if the actor looks before it lands.
                if deferred.is_some() {
                    unwritten.verdict_deferred();
                }
                unwritten.report(metrics);
                if let Some((in_tx, msg)) = deferred {
                    let _ = in_tx.send(msg).await;
                }
            }
            // The ordinary exits (peer gone, channel closed) still say
            // goodbye on the wire — under the same window, so a socket
            // that wedges on the way out cannot pin this task forever.
            None => {
                // After a failed write the channel may still hold batches
                // (counted, B66); after an ordinary end it is empty.
                unwritten.drain(&mut out_rx);
                drop(out_rx);
                // Counted before the goodbye: a close that wedges must
                // not hold the count back.
                unwritten.report(metrics);
                let _ = step(Op::close(&mut sink, progress), write_stall, &mut progress).await;
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
