//! The reader/writer pump tasks shared by all transports.
//!
//! This is the concrete "1 reader + 1 writer" pattern: two dedicated tokio
//! tasks per connection, each with exactly one thing to wait on (a stream
//! item or a channel message). No multiplexing anywhere.
//!
//! The reader may be wrapped in an **idle timeout**: when no client frame
//! arrives within the window, the pump notifies the connection actor via
//! [`ConnIn::ServerClosed`] and stops.
//!
//! That timeout is the session-lifecycle guardrail. A half-open TCP
//! connection (cable pulled, power lost, no FIN/RST) otherwise sits in the
//! reader forever, pinning its three tasks, two channels, and registry
//! entry. The timeout is also the *only* clock in the whole connection
//! path: the connection actor's only await stays its inbox `recv`, and no
//! per-connection timer task, registry message, or ticker subscription is
//! needed (see `docs/DESIGN.md`, session lifecycle).

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;

/// Spawn both pump tasks for a connection.
///
/// - `reader` yields decoded frames; when it errors, ends, or stays silent
///   for `idle_timeout`, the connection actor is notified via `in_tx`.
/// - `writer` consumes outbound batches and writes them to the socket.
///
/// `idle_timeout`: how long the reader may wait for the next *client*
/// frame before the server closes the connection on its own initiative.
/// Any inbound frame (a heartbeat, a move, anything) resets the window —
/// the pump re-wraps every read, so the deadline always starts at the
/// previous frame's arrival. `None` (or the composition root's
/// `Duration::ZERO` mapping) disables the check.
#[allow(clippy::type_complexity)]
pub fn spawn_pumps<Reader, Writer>(
    conn: ConnectionId,
    reader: Reader,
    writer: Writer,
    in_tx: Mailbox<ConnIn>,
    mut out_rx: Inbox<FrameBatch>,
    idle_timeout: Option<Duration>,
) -> (JoinHandle<()>, JoinHandle<()>)
where
    Reader: futures::Stream<Item = std::io::Result<FrameBody>> + Unpin + Send + 'static,
    Writer: futures::Sink<FrameBody, Error = std::io::Error> + Unpin + Send + 'static,
{
    let read = tokio::spawn(async move {
        let mut stream = reader;
        // The notification to send when the loop exits (if any).
        let mut exit_msg: Option<ConnIn> = None;
        loop {
            // One awaited source, optionally with a deadline. `timeout`
            // wraps a single future — it does not multiplex two live
            // sources: the deadline fires only while the read stays
            // pending, and a ready frame always wins (this is the same
            // idiom the load generator's client reads use).
            let item = match idle_timeout {
                Some(t) => match tokio::time::timeout(t, stream.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        warn!(
                            %conn,
                            timeout = ?t,
                            "reader pump: no client traffic for the idle window; \
                             server closing the connection"
                        );
                        exit_msg = Some(ConnIn::ServerClosed {
                            reason: format!("idle timeout: no client traffic for {t:?}"),
                        });
                        break;
                    }
                },
                None => stream.next().await,
            };
            let Some(result) = item else {
                // Clean end of stream (peer closed).
                exit_msg = Some(ConnIn::Closed {
                    reason: "peer closed".into(),
                });
                break;
            };
            match result {
                Ok(frame) => {
                    if in_tx.send(ConnIn::Frame(frame)).await.is_err() {
                        break; // connection actor is gone: nothing to tell
                    }
                }
                Err(e) => {
                    warn!(%conn, error = %e, "reader pump: io error");
                    exit_msg = Some(ConnIn::Closed {
                        reason: e.to_string(),
                    });
                    break;
                }
            }
        }
        if let Some(msg) = exit_msg {
            let _ = in_tx.send(msg).await;
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
