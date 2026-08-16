//! The reader/writer pump tasks shared by all transports.
//!
//! This is the concrete "1 reader + 1 writer" pattern: two dedicated tokio
//! tasks per connection, each with exactly one thing to wait on (a stream
//! item or a channel message). No multiplexing anywhere.

use futures::{SinkExt, StreamExt};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;

/// Spawn both pump tasks for a connection.
///
/// - `reader` yields decoded frames; when it errors or ends, the
///   connection actor is notified via `in_tx`.
/// - `writer` consumes outbound batches and writes them to the socket.
#[allow(clippy::type_complexity)]
pub fn spawn_pumps<Reader, Writer>(
    conn: ConnectionId,
    reader: Reader,
    writer: Writer,
    in_tx: Mailbox<ConnIn>,
    mut out_rx: Inbox<FrameBatch>,
) -> (JoinHandle<()>, JoinHandle<()>)
where
    Reader: futures::Stream<Item = std::io::Result<FrameBody>> + Unpin + Send + 'static,
    Writer: futures::Sink<FrameBody, Error = std::io::Error> + Unpin + Send + 'static,
{
    let read = tokio::spawn(async move {
        let mut stream = reader;
        let mut notified = false;
        while let Some(result) = stream.next().await {
            match result {
                Ok(frame) => {
                    if in_tx.send(ConnIn::Frame(frame)).await.is_err() {
                        break; // connection actor is gone
                    }
                }
                Err(e) => {
                    warn!(%conn, error = %e, "reader pump: io error");
                    let _ = in_tx
                        .send(ConnIn::Closed {
                            reason: e.to_string(),
                        })
                        .await;
                    notified = true;
                    break;
                }
            }
        }
        // Clean end of stream (peer closed) — or the actor already went.
        // Sent at most once: the error path above already notified.
        if !notified {
            let _ = in_tx
                .send(ConnIn::Closed {
                    reason: "peer closed".into(),
                })
                .await;
        }
        debug!(%conn, "reader pump stopped");
    });

    let write = tokio::spawn(async move {
        let mut sink = writer;
        while let Some(batch) = out_rx.recv().await {
            for frame in batch {
                if sink.send(frame).await.is_err() {
                    warn!(%conn, "writer pump: send failed; peer gone");
                    break;
                }
            }
            if sink.flush().await.is_err() {
                warn!(%conn, "writer pump: flush failed; peer gone");
                break;
            }
        }
        let _ = sink.close().await;
        debug!(%conn, "writer pump stopped");
    });

    (read, write)
}
