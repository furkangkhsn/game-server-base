//! The HTTP ops surface (`docs/OPS.md`): `/healthz`, `/metrics`,
//! `/rooms`, and the two room-admin writes, over a hand-rolled minimal
//! HTTP/1.1 on a tokio `TcpListener`.
//!
//! WHY hand-rolled (OPS decision 1): zero new dependencies; three read
//! endpoints and two writes do not justify a framework's dependency
//! budget and attack surface. Only what the surface needs is implemented:
//! one request per connection, `Connection: close`, no keep-alive, no
//! chunking, no JSON (OPS decision 3 — query-string parameters only).
//!
//! Discipline: the accept loop's ONLY awaited source is `accept()`. Each
//! connection gets a short-lived task that reads exactly one request head
//! (request line + headers, up to the CRLFCRLF terminator) within
//! `head::HEAD_DEADLINE` (else one 408, B47), ignores any body, writes
//! exactly one response with `Connection: close`, then closes — so no
//! multiplexed waits are needed anywhere and the actor discipline
//! survives unchanged. Every wait a peer controls is bounded: the head
//! read (B47), the response write (`http_write_timeout_secs`, B49) and
//! the drain; and so is their number — `http_max_connections` live
//! connection tasks (B49, `limits`). So is the one wait the server's own
//! state controls: the routing step's answers from the bookkeeper and
//! the registry (`http_route_timeout_secs`, B90 — a `504` past it).
//!
//! Stop (BACKLOG B33): the accept runs through a [`Door`], the one every
//! game listener closes (B16). `ServerHandle::stop` closes it, the
//! pending accept ends with the listener-closed error, the loop returns
//! and drops the listener; `stop` joins it with the game listeners'
//! loops and aborts it only as the same backstop. Connections already
//! accepted keep their own short-lived tasks (one response, bounded
//! drain), as before.
//!
//! State discipline (no shared mutable state): the latest metrics report
//! travels through the collector's `watch` channel (single writer;
//! readers take a synchronous `borrow()` snapshot — latest-wins, a slow
//! scraper can never back anyone up); room mutations travel through the
//! registry actor's mailbox — exactly the control plane the wire protocol
//! uses, no new control path (OPS §2). The *listing* of known room ids
//! lives in one small bookkeeper task that owns its set as task-local
//! state and answers queries over its own channel.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use gsb_core::channel::Mailbox;
use gsb_core::id::RoomId;
use gsb_core::metrics::MetricReport;
use gsb_core::registry::RegistryMsg;
use gsb_net::transport::{Door, is_listener_closed};

use crate::config::RoomTemplate;

mod head;
mod limits;
mod response;
mod routes;
use head::*;
use limits::*;
pub(crate) use limits::{OpsGuard, OpsLimits};
use response::Response;
use routes::*;

#[cfg(test)]
mod tests;

/// `/healthz` freshness threshold, in report periods: a report older than
/// this many collector periods means the metrics ticker (and therefore the
/// process's heartbeat) has stalled — 503 with the age in the body (OPS §2).
const STALE_AFTER_PERIODS: u32 = 3;

/// How long the per-connection task lingers after its response draining
/// whatever the peer still had in flight (e.g. a POST body we ignore).
/// WHY drain at all: closing a socket with unread inbound data makes some
/// stacks emit RST, which can truncate the already-written response on the
/// client side. A bounded drain gives well-behaved peers a clean FIN path
/// without letting a silent peer hold a task forever.
const DRAIN_WINDOW: Duration = Duration::from_millis(300);

/// One message to the room-listing bookkeeper (the single owner of "which
/// room ids does this surface know about").
enum RoomsMsg {
    /// An id successfully opened through this surface: include it in every
    /// future listing.
    Seen(RoomId),
    /// A listing query; the reply carries the known ids (sorted).
    Listing(oneshot::Sender<Vec<u64>>),
}

/// Everything a connection task needs. Cheaply cloneable (a watch receiver
/// clone and a mailbox clone — both Arc-backed), cloned once per accepted
/// connection.
#[derive(Clone)]
pub(crate) struct OpsHttp {
    reports: watch::Receiver<MetricReport>,
    registry: Mailbox<RegistryMsg>,
    rooms: mpsc::Sender<RoomsMsg>,
    period: Duration,
    /// The server's room: what `POST /rooms/open` builds (the same
    /// template the pre-created rooms come from — the surface never
    /// invents room settings of its own).
    room_template: RoomTemplate,
    /// The connection cap and the write deadline (B49).
    limits: OpsLimits,
    /// What the accept loop and the connection tasks count (B49).
    counters: Arc<OpsCounters>,
}

/// The running ops surface: its accept loop and the door that ends it.
pub(crate) struct OpsSurface {
    /// Closing it ends the accept loop (which drops the listener).
    pub(crate) door: Arc<Door>,
    /// The accept loop; it returns once `door` is closed.
    pub(crate) task: JoinHandle<()>,
}

/// Spawn the ops surface: the bookkeeper plus the accept loop, which runs
/// until the returned door is closed (the bookkeeper ends once the loop
/// and its connection tasks have dropped their senders).
pub(crate) fn spawn(
    listener: TcpListener,
    registry: Mailbox<RegistryMsg>,
    reports: watch::Receiver<MetricReport>,
    period: Duration,
    room_template: RoomTemplate,
    configured_rooms: impl Iterator<Item = u64>,
    guard: OpsGuard,
) -> OpsSurface {
    let OpsGuard { limits, metrics } = guard;
    let (rooms_tx, rooms_rx) = mpsc::channel::<RoomsMsg>(16);
    // Bounded (the project's backpressure discipline): the bookkeeper is a
    // trivial task, so 16 slots never fill at admin frequency; a full slot
    // would drop only a listing notice, never a mutation.
    let known: BTreeSet<u64> = configured_rooms.collect();
    tokio::spawn(run_room_bookkeeper(rooms_rx, known));
    let ops = OpsHttp {
        reports,
        registry,
        rooms: rooms_tx,
        period,
        room_template,
        limits,
        counters: Arc::new(OpsCounters::default()),
    };
    let door = Arc::new(Door::new());
    let task = tokio::spawn(accept_loop(listener, ops, Arc::clone(&door), metrics));
    OpsSurface { door, task }
}

/// The accept loop. Its ONLY awaited source is `accept()` (through the
/// door, which can only end it) — everything else happens inside
/// short-lived per-connection tasks, at most `http_max_connections` of
/// them (B49). Sends the surface's counts to the collector after an
/// accept (when due) and once more as it ends. Returns on the closed
/// door.
async fn accept_loop(
    listener: TcpListener,
    ops: OpsHttp,
    door: Arc<Door>,
    metrics: gsb_net::TransportMetrics,
) {
    let mut flusher = gsb_net::Flusher::new(metrics);
    // Set by the first refusal of a saturated spell (one warning per
    // spell, not one per refused connection); cleared by the next slot.
    let mut saturated = false;
    loop {
        match door.admit(listener.accept()).await {
            Ok((stream, peer)) => match ops.counters.try_slot(ops.limits.max_connections) {
                Some(slot) => {
                    debug!(%peer, "ops http connection");
                    saturated = false;
                    let ops = ops.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        serve_one(stream, ops).await;
                    });
                }
                // Refused: the socket closes as it drops, unanswered.
                None => {
                    if !std::mem::replace(&mut saturated, true) {
                        warn!(
                            max = ?ops.limits.max_connections,
                            "ops http connections at the cap; refusing new ones"
                        );
                    }
                    debug!(%peer, "ops http connection refused at the cap");
                }
            },
            Err(e) if is_listener_closed(&e) => {
                debug!("ops http door closed; accept loop ends");
                flusher.flush(ops.counters.totals(), true);
                return;
            }
            Err(e) => {
                warn!(%e, "ops http accept error; backing off");
                // A persistent error (EMFILE …) must not become a spin loop.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        if flusher.due() {
            flusher.flush(ops.counters.totals(), false);
        }
    }
}

/// One request, one response, close. Generic over the stream so a test
/// can drive it over an in-memory pipe under a paused clock.
async fn serve_one<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, ops: OpsHttp) {
    let response = match read_head_in_time(&mut stream).await {
        Ok(head) => route_in_time(&head, &ops).await,
        Err(HeadError::TooLarge) => Response::text(
            431,
            "Request Header Fields Too Large",
            "request head too large\n",
        ),
        Err(HeadError::Malformed) => {
            Response::text(400, "Bad Request", "malformed or truncated request\n")
        }
        // The one bounded answer to a peer that never finished its head
        // (B47); the close below then ends the task.
        Err(HeadError::TimedOut) => Response::text(
            408,
            "Request Timeout",
            "request head not received in time\n",
        ),
    };
    let bytes = response.serialize();
    let write = async {
        stream.write_all(&bytes).await?;
        let _ = stream.shutdown().await;
        Ok::<(), std::io::Error>(())
    };
    // One deadline around the whole write (B49): a peer that does not
    // read its answer cannot hold the task.
    let written = match ops.limits.write_timeout {
        Some(deadline) => match tokio::time::timeout(deadline, write).await {
            Ok(done) => done,
            Err(_) => {
                ops.counters.count_write_timeout();
                warn!(timeout = ?deadline, "ops http response not read in time; closing");
                return;
            }
        },
        None => write.await,
    };
    if written.is_err() {
        return; // the peer left before the answer; nothing to serve anymore
    }
    // Best-effort bounded drain (see DRAIN_WINDOW for why it exists).
    let _ = tokio::time::timeout(DRAIN_WINDOW, async {
        let mut sink = [0u8; 512];
        loop {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
}
