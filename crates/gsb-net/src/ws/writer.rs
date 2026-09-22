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
    /// One complete game frame's `[u32 LE len][op][payload]` envelope, to
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
/// nothing writes.
pub(super) fn spawn_socket_writer(sock: OwnedWriteHalf) -> (mpsc::Sender<WsOut>, Arc<AtomicU64>) {
    let (tx, rx) = mpsc::channel::<WsOut>(OUT_QUEUE_CAPACITY);
    let written = Arc::new(AtomicU64::new(0));
    tokio::spawn(ws_writer_task(sock, rx, Arc::clone(&written)));
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
) {
    'queue: while let Some(out) = rx.recv().await {
        let bytes = match out {
            WsOut::Game(envelope) => Bytes::from(encode_server_frame(OP_BIN, &envelope)),
            WsOut::Control(op, payload) => Bytes::from(encode_server_frame(op, &payload)),
            // Both halves drop here: the peer sees a prompt TCP FIN.
            WsOut::Shutdown => break,
        };
        let mut off = 0;
        while off < bytes.len() {
            match sock.write(&bytes[off..]).await {
                // `Ok(0)` is `write_all`'s WriteZero error: the socket is
                // done taking bytes.
                Ok(0) | Err(_) => break 'queue,
                Ok(n) => {
                    off += n;
                    written.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    }
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
        closing: Arc<AtomicBool>,
        written: Arc<AtomicU64>,
    ) -> Self {
        Self {
            tx: PollSender::new(tx),
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
        self.get_mut()
            .tx
            .send_item(WsOut::Game(encode_game_envelope(&item)))
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
        let this = self.get_mut();
        if !this.closing.swap(true, Ordering::SeqCst)
            && let Some(tx) = this.tx.get_ref()
        {
            let _ = tx.try_send(WsOut::Control(OP_CLOSE, Vec::new()));
        }
        Poll::Ready(Ok(()))
    }
}

pub(super) fn writer_gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "websocket writer task is gone")
}
