//! The write half: one bounded queue drained by one writer task, so
//! control frames and game batches share a single socket owner.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

use bytes::Bytes;
use futures::Sink;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;

use gsb_protocol::FrameBody;

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

/// The ONLY task that ever writes to the socket. It awaits exactly one
/// source — the queue — which merges the writer pump's game traffic with
/// the reader's control replies (no multiplexing anywhere).
pub(super) async fn ws_writer_task(mut sock: OwnedWriteHalf, mut rx: mpsc::Receiver<WsOut>) {
    while let Some(out) = rx.recv().await {
        let bytes = match out {
            WsOut::Game(envelope) => Bytes::from(encode_server_frame(OP_BIN, &envelope)),
            WsOut::Control(op, payload) => Bytes::from(encode_server_frame(op, &payload)),
            // Both halves drop here: the peer sees a prompt TCP FIN.
            WsOut::Shutdown => break,
        };
        if sock.write_all(&bytes).await.is_err() {
            break;
        }
    }
    // Last queue end dropped, write failed, or shutdown requested: shut the
    // socket down so a lingering peer observes EOF promptly.
    let _ = sock.shutdown().await;
}

/// Pump-facing writer: pushes encoded game frames onto the shared outbound
/// queue. Flushing is implicit — once queued, the writer task owns delivery.
/// Backpressure comes from the bounded queue via a reserved-capacity permit
/// taken in `poll_ready` and spent in `start_send`. `closing` guarantees at
/// most ONE close frame ever leaves this connection (the read path's echo
/// or failure-close wins; the sink's teardown close only fires otherwise).
pub(super) struct WsWriter {
    pub(super) tx: mpsc::Sender<WsOut>,
    pub(super) permit: Option<mpsc::OwnedPermit<WsOut>>,
    pub(super) closing: Arc<AtomicBool>,
}

impl Sink<FrameBody> for WsWriter {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.permit.is_some() {
            return Poll::Ready(Ok(()));
        }
        // `reserve_owned` takes the sender by value, so hand it a cheap
        // clone (an Arc bump); the permit itself is what gets stored.
        let mut reserve = std::pin::pin!(this.tx.clone().reserve_owned());
        match ready!(reserve.as_mut().poll(cx)) {
            Ok(permit) => {
                this.permit = Some(permit);
                Poll::Ready(Ok(()))
            }
            Err(_) => Poll::Ready(Err(writer_gone())),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: FrameBody) -> io::Result<()> {
        let this = self.get_mut();
        match this.permit.take() {
            Some(permit) => {
                // `send` hands the sender back (chaining API); drop it.
                let _ = permit.send(WsOut::Game(encode_game_envelope(&item)));
                Ok(())
            }
            // Unreachable after a successful poll_ready; a defensive error
            // beats a panic either way.
            None => {
                use tokio::sync::mpsc::error::TrySendError;
                match this.tx.try_send(WsOut::Game(encode_game_envelope(&item))) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Full(_)) => Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "websocket outbound queue full",
                    )),
                    Err(TrySendError::Closed(_)) => Err(writer_gone()),
                }
            }
        }
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
        if !this.closing.swap(true, Ordering::SeqCst) {
            let _ = this.tx.try_send(WsOut::Control(OP_CLOSE, Vec::new()));
        }
        Poll::Ready(Ok(()))
    }
}

pub(super) fn writer_gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "websocket writer task is gone")
}
