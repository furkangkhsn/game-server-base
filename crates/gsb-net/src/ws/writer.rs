//! The write half: one bounded queue drained by one writer task, so
//! control frames and game batches share a single socket owner.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

use bytes::Bytes;
use futures::Sink;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;
use tokio_util::sync::PollSender;

use gsb_protocol::FrameBody;

use crate::pump::WriteProgress;
use crate::ws::*;

/// Outbound work for the single socket-writer task.
pub(super) enum WsOut {
    /// One complete game frame's `[u32 LE len][op][payload]` envelope (or,
    /// under the opaque mapping, the bare payload), to
    /// go out as ONE unmasked FIN binary message (the WS frame layer is
    /// applied here, at the only place that touches the socket).
    Game(Bytes),
    /// A control reply from the read path (pong / close / failure close):
    /// `(opcode, payload)`.
    Control(u8, Vec<u8>),
    /// Stop writing and shut the socket down: the peer initiated the close
    /// handshake and we echoed it — RFC 6455 §7.1.1 has the server close
    /// FIRST, not wait for the actor layer's teardown.
    Shutdown,
}

/// Spawn the door's socket-writer task. Returns the queue into it and
/// the byte count it keeps — the SAME `Arc` the task bumps, handed out
/// from one place so the pump's writer can never be wired to a count
/// nothing writes. `metrics`: where the frames it never writes are
/// counted (B66, [`lost`]).
pub(super) fn spawn_socket_writer(
    sock: OwnedWriteHalf,
    metrics: crate::TransportMetrics,
) -> (mpsc::Sender<WsOut>, Arc<AtomicU64>) {
    let (tx, rx) = mpsc::channel::<WsOut>(OUT_QUEUE_CAPACITY);
    let written = Arc::new(AtomicU64::new(0));
    tokio::spawn(ws_writer_task(sock, rx, Arc::clone(&written), metrics));
    (tx, written)
}

/// The ONLY task that ever writes to the socket. It awaits exactly one
/// source — the queue — which merges the writer pump's game traffic with
/// the reader's control replies (no multiplexing anywhere).
///
/// `written` is the write-stall clock's byte signal for this door (see
/// [`WsWriter`]'s [`WriteProgress`]): this task is the only writer of it,
/// bumping it on every partial socket write — which is why a frame is
/// written with a `write` loop rather than `write_all`, whose single
/// future would only say "done" once the whole frame is out.
async fn ws_writer_task(
    mut sock: OwnedWriteHalf,
    mut rx: mpsc::Receiver<WsOut>,
    written: Arc<AtomicU64>,
    metrics: crate::TransportMetrics,
) {
    // RFC 6455 §5.5.1: no DATA frame may follow a Close frame. Enforced
    // here, the only place that sees the real wire order: on a refused
    // stream the reader queues its failure close first, and the actor's
    // close notice (`ERROR` 9) and any fan-out still in flight land
    // behind it. The close frame IS this door's notice; they are dropped.
    // Nor does a second close follow the first: after the server's own
    // teardown close (1001) the client's answering close must not be
    // echoed (§5.5.1: an endpoint echoes a close only if it did not send
    // one first) — the reader still queues the echo and the shutdown,
    // and only the shutdown is acted on.
    let mut close_sent = false;
    // What never reaches the wire (B66), and whether the loop left early.
    let mut lost = lost::Lost::default();
    let mut early = false;
    let mut failed = false;
    'queue: while let Some(out) = rx.recv().await {
        let game = matches!(out, WsOut::Game(_));
        let bytes = match out {
            WsOut::Game(_) if close_sent => {
                lost.after_close();
                continue;
            }
            WsOut::Control(..) if close_sent => continue,
            WsOut::Game(envelope) => Bytes::from(encode_server_frame(OP_BIN, &envelope)),
            WsOut::Control(op, payload) => {
                close_sent |= op == OP_CLOSE;
                Bytes::from(encode_server_frame(op, &payload))
            }
            // Both halves drop here: the peer sees a prompt TCP FIN.
            WsOut::Shutdown => {
                early = true;
                break;
            }
        };
        let mut off = 0;
        while off < bytes.len() {
            match sock.write(&bytes[off..]).await {
                // `Ok(0)` is `write_all`'s WriteZero error: the socket is
                // done taking bytes.
                Ok(0) | Err(_) => {
                    lost.failed(game);
                    (early, failed) = (true, true);
                    break 'queue;
                }
                Ok(n) => {
                    off += n;
                    written.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    }
    // An early exit leaves the queue's contents unwritten: counted before
    // the queue goes (a failed close frame never reached the wire, so
    // nothing behind it is "after a close").
    if early {
        lost.drain(&mut rx, close_sent && !failed);
    }
    lost.report(metrics);
    // Last queue end dropped, write failed, or shutdown requested: shut the
    // socket down so a lingering peer observes EOF promptly.
    let _ = sock.shutdown().await;
}

/// Pump-facing writer: pushes encoded game frames onto the shared outbound
/// queue. Flushing is implicit — once queued, the writer task owns delivery.
/// Backpressure comes from the bounded queue via a capacity reservation
/// taken in `poll_ready` and spent in `start_send`. `closing` guarantees at
/// most ONE close frame ever leaves this connection (the read path's echo
/// or failure-close wins; the sink's teardown close only fires otherwise).
///
/// The reservation is a [`PollSender`], which keeps its pending reserve
/// future across polls. That is load-bearing: a reserve future that is
/// built afresh on each `poll_ready` and dropped on `Pending` takes its
/// waiter off the channel's wait list, so the slot freeing up later wakes
/// nobody — the pump then sleeps until something else happens to poll it
/// (with the stall clock off: forever).
pub(super) struct WsWriter {
    tx: PollSender<WsOut>,
    mapping: WsMessageMapping,
    closing: Arc<AtomicBool>,
    /// Bytes the socket-writer task has written (it is the only writer;
    /// this side only reads). See [`WriteProgress`] below.
    written: Arc<AtomicU64>,
}

impl WsWriter {
    /// `tx` and `written` are what [`spawn_socket_writer`] returned;
    /// `closing` is shared with the read path.
    pub(super) fn new(
        tx: mpsc::Sender<WsOut>,
        mapping: WsMessageMapping,
        closing: Arc<AtomicBool>,
        written: Arc<AtomicU64>,
    ) -> Self {
        Self {
            tx: PollSender::new(tx),
            mapping,
            closing,
            written,
        }
    }
}

/// The WS door's byte signal comes from the socket-writer TASK, not from
/// this sink: the pump's sends here complete when a QUEUE slot frees,
/// i.e. when the task has finished an earlier frame — counting those
/// would be the frame-granular clock again, one queue away. A relaxed
/// load is enough: only a change is ever looked for.
impl WriteProgress for WsWriter {
    fn bytes_written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }
}

impl Sink<FrameBody> for WsWriter {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Registers this task as a waiter that STAYS registered while
        // Pending (see the type docs), and is a no-op when a slot is
        // already reserved.
        self.get_mut()
            .tx
            .poll_reserve(cx)
            .map_err(|_| writer_gone())
    }

    fn start_send(self: Pin<&mut Self>, item: FrameBody) -> io::Result<()> {
        // Spends the slot `poll_ready` reserved. Without one (a caller
        // that skipped `poll_ready`) this is an error, never a panic.
        let this = self.get_mut();
        let message = match this.mapping {
            WsMessageMapping::GameEnvelope => encode_game_envelope(&item),
            WsMessageMapping::Opaque => item.payload,
        };
        this.tx
            .send_item(WsOut::Game(message))
            .map_err(|_| writer_gone())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Best-effort WS close notice — but only if the read path has not
        // already sent one (echo / failure close): a second close frame is
        // noise. The actual socket shutdown happens when every queue end is
        // gone (writer task then shuts the half).
        //
        // This close is the SERVER leaving the session (the actor ended:
        // `stop()`, or a verdict whose `ERROR` frame went out just before),
        // so it carries 1001 "Going Away" — the status code alone, no
        // reason text. An empty close frame reads as 1005 ("no status")
        // on the client, indistinguishable from a peer that said nothing.
        let this = self.get_mut();
        if !this.closing.swap(true, Ordering::SeqCst)
            && let Some(tx) = this.tx.get_ref()
        {
            let _ = tx.try_send(WsOut::Control(
                OP_CLOSE,
                CLOSE_GOING_AWAY.to_be_bytes().to_vec(),
            ));
        }
        Poll::Ready(Ok(()))
    }
}

pub(super) fn writer_gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "websocket writer task is gone")
}

/// The socket writer's losses on their way to the collector (B66). A
/// CHILD module, so it reaches [`WsOut`] directly.
mod lost;
