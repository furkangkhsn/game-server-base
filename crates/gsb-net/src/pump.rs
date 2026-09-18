//! The reader/writer pump tasks shared by all transports.
//!
//! This is the concrete "1 reader + 1 writer" pattern: two dedicated tokio
//! tasks per connection, each with exactly one thing to wait on (a stream
//! item or a channel message). No multiplexing anywhere.
//!
//! Each pump may be wrapped in a deadline, and the two are a PAIR — one
//! per direction of the socket, the two halves of the session-lifecycle
//! guardrail (see [`PumpTimeouts`]):
//!
//! - the reader's **idle timeout**: when no client frame arrives within
//!   the window, the pump notifies the connection actor via
//!   [`ConnIn::ServerClosed`] and stops;
//! - the writer's **write stall**: when no write to the socket COMPLETES
//!   within the window, the same notification is sent (see [`writer`]).
//!
//! The reader's timeout covers a half-open TCP connection (cable pulled,
//! power lost, no FIN/RST) that would otherwise sit in the reader forever,
//! pinning its three tasks, two channels, and registry entry. The writer's
//! covers the peer that is still there and simply stops reading: its
//! receive window closes, the writer parks inside the socket write, the
//! outbound channel stays FULL (never closed — so the `w_closing` teardown
//! never fires), the room drops a frame every tick, and the session lives
//! on holding its room slot and registry row while receiving nothing.
//! Inbound silence cannot see that, because there need not be any.
//!
//! Together they are also the *only* clocks in the whole connection path:
//! the connection actor's only await stays its inbox `recv`, and no
//! per-connection timer task, registry message, or ticker subscription is
//! needed (see `docs/DESIGN.md`, session lifecycle).

mod writer;

use std::time::Duration;

use futures::StreamExt;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;

/// The two session-lifecycle deadlines the pumps run, one per direction.
///
/// Both are expressed in TIME, never in ticks or frame counts: a low-Hz
/// room must not kill a connection faster than a high-Hz one, and neither
/// window may depend on how much traffic happens to be flowing.
///
/// `None` disables that direction's check (the composition root maps its
/// `0` config spelling here).
#[derive(Debug, Clone, Copy, Default)]
pub struct PumpTimeouts {
    /// How long the reader may wait for the next *client* frame before
    /// the server closes the connection on its own initiative. Any
    /// inbound frame (a heartbeat, a move, anything) resets the window —
    /// the pump re-wraps every read, so the deadline always starts at the
    /// previous frame's arrival.
    pub idle: Option<Duration>,
    /// How long the writer may go without a single COMPLETED write to the
    /// socket while it has something to write. Progress, not age: every
    /// successful frame write (and flush) restarts the window, so a
    /// client that is merely BEHIND — still draining bytes, just slowly —
    /// never trips it, while a client that has stopped draining entirely
    /// does. An idle outbound path is not a stall either: the window
    /// restarts whenever the pump is waiting for work.
    pub write_stall: Option<Duration>,
}

/// Spawn both pump tasks for a connection.
///
/// - `reader` yields decoded frames; when it errors, ends, or stays silent
///   for `timeouts.idle`, the connection actor is notified via `in_tx`.
/// - `writer` consumes outbound batches and writes them to the socket;
///   when no write completes for `timeouts.write_stall`, the connection
///   actor is notified through the SAME mailbox (see [`writer`]).
pub fn spawn_pumps<Reader, Writer>(
    conn: ConnectionId,
    reader: Reader,
    writer: Writer,
    in_tx: Mailbox<ConnIn>,
    out_rx: Inbox<FrameBatch>,
    timeouts: PumpTimeouts,
) -> (JoinHandle<()>, JoinHandle<()>)
where
    Reader: futures::Stream<Item = std::io::Result<FrameBody>> + Unpin + Send + 'static,
    Writer: futures::Sink<FrameBody, Error = std::io::Error> + Unpin + Send + 'static,
{
    // The writer gets its OWN handle on the actor's mailbox: its verdict
    // travels that IN-PROCESS channel, never the socket — which is
    // precisely the thing that is stuck when it has a verdict to deliver.
    let write = writer::spawn(conn, writer, in_tx.clone(), out_rx, timeouts.write_stall);
    let idle_timeout = timeouts.idle;

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

    (read, write)
}
