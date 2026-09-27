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
//! (request line + headers, up to the CRLFCRLF terminator), ignores any
//! body, writes exactly one response with `Connection: close`, then
//! closes — so no multiplexed waits are needed anywhere and the actor
//! discipline survives unchanged.
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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use gsb_core::channel::Mailbox;
use gsb_core::id::RoomId;
use gsb_core::metrics::MetricReport;
use gsb_core::registry::RegistryMsg;
use gsb_net::transport::{Door, is_listener_closed};

use crate::config::RoomTemplate;

mod routes;
use routes::*;

#[cfg(test)]
mod tests;

/// `/healthz` freshness threshold, in report periods: a report older than
/// this many collector periods means the metrics ticker (and therefore the
/// process's heartbeat) has stalled — 503 with the age in the body (OPS §2).
const STALE_AFTER_PERIODS: u32 = 3;

/// Request-head cap in bytes. A head that outgrows it is answered 431 and
/// dropped: the surface serves operators, not uploads, and an unbounded
/// read would be a memory-amplification bug for anyone who can reach the
/// port.
const MAX_HEAD_BYTES: usize = 8 * 1024;

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
) -> OpsSurface {
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
    };
    let door = Arc::new(Door::new());
    let task = tokio::spawn(accept_loop(listener, ops, Arc::clone(&door)));
    OpsSurface { door, task }
}

/// The accept loop. Its ONLY awaited source is `accept()` (through the
/// door, which can only end it) — everything else happens inside
/// short-lived per-connection tasks. Returns on the closed door.
async fn accept_loop(listener: TcpListener, ops: OpsHttp, door: Arc<Door>) {
    loop {
        match door.admit(listener.accept()).await {
            Ok((stream, peer)) => {
                debug!(%peer, "ops http connection");
                let ops = ops.clone();
                tokio::spawn(serve_one(stream, ops));
            }
            Err(e) if is_listener_closed(&e) => {
                debug!("ops http door closed; accept loop ends");
                return;
            }
            Err(e) => {
                warn!(%e, "ops http accept error; backing off");
                // A persistent error (EMFILE …) must not become a spin loop.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// One request, one response, close.
async fn serve_one(mut stream: TcpStream, ops: OpsHttp) {
    let response = match read_head(&mut stream).await {
        Ok(head) => route(&head, &ops).await,
        Err(HeadError::TooLarge) => Response::text(
            431,
            "Request Header Fields Too Large",
            "request head too large\n",
        ),
        Err(HeadError::Malformed) => {
            Response::text(400, "Bad Request", "malformed or truncated request\n")
        }
    };
    if stream.write_all(&response.serialize()).await.is_err() {
        return; // the peer left before the answer; nothing to serve anymore
    }
    let _ = stream.shutdown().await;
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

enum HeadError {
    Malformed,
    TooLarge,
}

/// Read exactly one request head: bytes up to (not including) the CRLFCRLF
/// terminator. Any body is ignored by construction (we stop reading at the
/// terminator and answer `Connection: close`).
async fn read_head(stream: &mut TcpStream) -> Result<String, HeadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = find_head_end(&buf) {
            return String::from_utf8(buf[..end].to_vec()).map_err(|_| HeadError::Malformed);
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(HeadError::TooLarge);
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| HeadError::Malformed)?;
        if n == 0 {
            // EOF before the head ended: not a request.
            return Err(HeadError::Malformed);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// One response: status line + the minimal header set + a text body.
struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    allow: Option<&'static str>,
    body: String,
}

impl Response {
    fn text(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            allow: None,
            body: body.into(),
        }
    }

    fn method_not_allowed(allow: &'static str) -> Self {
        let mut r = Self::text(405, "Method Not Allowed", "method not allowed\n");
        r.allow = Some(allow);
        r
    }

    fn serialize(&self) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut head = String::with_capacity(160 + self.body.len());
        let _ = write!(
            head,
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        if let Some(allow) = self.allow {
            let _ = write!(head, "Allow: {allow}\r\n");
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(self.body.as_bytes());
        out
    }
}
