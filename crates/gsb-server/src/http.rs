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
//! State discipline (no shared mutable state): the latest metrics report
//! travels through the collector's `watch` channel (single writer;
//! readers take a synchronous `borrow()` snapshot — latest-wins, a slow
//! scraper can never back anyone up); room mutations travel through the
//! registry actor's mailbox — exactly the control plane the wire protocol
//! uses, no new control path (OPS §2). The *listing* of known room ids
//! lives in one small bookkeeper task that owns its set as task-local
//! state and answers queries over its own channel.

use std::collections::BTreeSet;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use gsb_core::channel::Mailbox;
use gsb_core::error::CoreError;
use gsb_core::id::RoomId;
use gsb_core::metrics::MetricReport;
use gsb_core::registry::{RegistryMsg, RoomStatus};
use gsb_core::room::RoomConfig;

use crate::{registry_close_room, registry_open_room, registry_room_status};

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
    default_tick_hz: f64,
}

/// Spawn the ops surface: the bookkeeper plus the accept loop (whose join
/// handle is returned; aborting it stops the surface — the listener drops
/// with the aborted future).
pub(crate) fn spawn(
    listener: TcpListener,
    registry: Mailbox<RegistryMsg>,
    reports: watch::Receiver<MetricReport>,
    period: Duration,
    default_tick_hz: f64,
    configured_rooms: impl Iterator<Item = u64>,
) -> JoinHandle<()> {
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
        default_tick_hz,
    };
    tokio::spawn(accept_loop(listener, ops))
}

/// The accept loop. Its ONLY awaited source is `accept()` — everything else
/// happens inside short-lived per-connection tasks.
async fn accept_loop(listener: TcpListener, ops: OpsHttp) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!(%peer, "ops http connection");
                let ops = ops.clone();
                tokio::spawn(serve_one(stream, ops));
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

/// Route one parsed request head. Known paths with the wrong verb answer
/// 405 (the resource exists, the method does not); unknown paths answer
/// 404; anything unparseable never gets here (`read_head` said 400).
async fn route(head: &str, ops: &OpsHttp) -> Response {
    let Some(line) = head.lines().next() else {
        return Response::text(400, "Bad Request", "empty request\n");
    };
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Response::text(400, "Bad Request", "malformed request line\n");
    };
    match parts.next() {
        Some(version) if version.starts_with("HTTP/1.") => {}
        _ => return Response::text(400, "Bad Request", "missing or unsupported HTTP version\n"),
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    if !path.starts_with('/') {
        return Response::text(400, "Bad Request", "request path must start with '/'\n");
    }
    match path {
        "/healthz" => {
            if method == "GET" {
                healthz(ops)
            } else {
                Response::method_not_allowed("GET")
            }
        }
        "/metrics" => {
            if method == "GET" {
                metrics_snapshot(ops)
            } else {
                Response::method_not_allowed("GET")
            }
        }
        "/rooms" => {
            if method == "GET" {
                list_rooms(ops).await
            } else {
                Response::method_not_allowed("GET")
            }
        }
        "/rooms/open" => {
            if method == "POST" {
                open_room(query, ops).await
            } else {
                Response::method_not_allowed("POST")
            }
        }
        "/rooms/close" => {
            if method == "POST" {
                close_room(query, ops).await
            } else {
                Response::method_not_allowed("POST")
            }
        }
        _ => Response::text(404, "Not Found", "not found\n"),
    }
}

/// GET /healthz: 200 while the metrics ticker is demonstrably alive (the
/// latest report's emission stamp is younger than three periods), 503 with
/// the age otherwise. The watch's initial value is born five periods old,
/// so a starting server honestly reports "warming up" until the first real
/// report instead of answering ok from a report nobody produced.
fn healthz(ops: &OpsHttp) -> Response {
    let report = ops.reports.borrow();
    let age = report.emitted_at.elapsed();
    let threshold = ops.period * STALE_AFTER_PERIODS;
    if age <= threshold {
        Response::text(200, "OK", "ok\n")
    } else {
        Response::text(
            503,
            "Service Unavailable",
            format!(
                "stale: last metrics report {:.3}s ago (threshold {:.3}s)\n",
                age.as_secs_f64(),
                threshold.as_secs_f64()
            ),
        )
    }
}

/// GET /metrics: the Prometheus text exposition (version 0.0.4) of the
/// latest borrowed snapshot. Rendering is synchronous over the borrow — no
/// await between `borrow()` and the last read, so the guard can never leak
/// across a wait point.
fn metrics_snapshot(ops: &OpsHttp) -> Response {
    let body = ops.reports.borrow().render_prometheus();
    Response {
        status: 200,
        reason: "OK",
        content_type: "text/plain; version=0.0.4",
        allow: None,
        body,
    }
}

/// GET /rooms: the known room ids (the configured ones plus everything
/// opened through this surface) with their CURRENT registry statuses. The
/// ids come from the bookkeeper; the statuses come from the registry — so
/// a room closed elsewhere shows up as absent here too.
async fn list_rooms(ops: &OpsHttp) -> Response {
    let (reply, reply_rx) = oneshot::channel();
    if ops.rooms.send(RoomsMsg::Listing(reply)).await.is_err() {
        return Response::text(503, "Service Unavailable", "room bookkeeper unavailable\n");
    }
    let ids = match reply_rx.await {
        Ok(ids) => ids,
        Err(_) => {
            return Response::text(503, "Service Unavailable", "room bookkeeper unavailable\n");
        }
    };
    let mut body = String::new();
    for id in ids {
        let status = match registry_room_status(&ops.registry, RoomId(id)).await {
            Ok(status) => render_status(status),
            Err(e) => format!("unknown ({e})"),
        };
        body.push_str(&format!("r{id} {status}\n"));
    }
    if body.is_empty() {
        body.push_str("(no rooms)\n");
    }
    Response::text(200, "OK", body)
}

/// POST /rooms/open?id=&tick_hz= → the registry's idempotent create. The
/// reply renders the resulting [`RoomStatus`] (a create round trip doubles
/// as a status query). `tick_hz` omitted = the server's global rate.
async fn open_room(query: &str, ops: &OpsHttp) -> Response {
    let Some(id) = required_id(query) else {
        return Response::text(
            400,
            "Bad Request",
            "missing or invalid `id` (expected a positive integer)\n",
        );
    };
    let tick_hz = match optional_f64(query, "tick_hz") {
        Ok(tick_hz) => tick_hz.unwrap_or(ops.default_tick_hz),
        Err(msg) => return Response::text(400, "Bad Request", msg),
    };
    let config = RoomConfig {
        id: RoomId(id),
        tick_hz,
        ..RoomConfig::default()
    };
    match registry_open_room(&ops.registry, config).await {
        Ok(status) => {
            // Record for future listings. try_send on a bounded channel: a
            // dropped notice (never expected at admin frequency) costs one
            // stale listing entry, never a mutation.
            let _ = ops.rooms.try_send(RoomsMsg::Seen(RoomId(id)));
            Response::text(200, "OK", format!("r{id} {}\n", render_status(status)))
        }
        Err(CoreError::RoomConflict(_)) => Response::text(
            409,
            "Conflict",
            format!("r{id} conflict: the room exists with a different configuration\n"),
        ),
        Err(CoreError::TickRate { room, global }) => Response::text(
            400,
            "Bad Request",
            format!("tick_hz {room} does not divide the global rate {global}\n"),
        ),
        Err(CoreError::KeepaliveRate { .. }) | Err(CoreError::InvalidTickRate { .. }) => {
            Response::text(400, "Bad Request", "invalid room parameters\n")
        }
        Err(e) => Response::text(503, "Service Unavailable", format!("{e}\n")),
    }
}

/// POST /rooms/close?id= → the registry's idempotent destroy (closing an
/// absent room is reported as absent, not an error).
async fn close_room(query: &str, ops: &OpsHttp) -> Response {
    let Some(id) = required_id(query) else {
        return Response::text(
            400,
            "Bad Request",
            "missing or invalid `id` (expected a positive integer)\n",
        );
    };
    match registry_close_room(&ops.registry, RoomId(id)).await {
        Ok(status) => Response::text(200, "OK", format!("r{id} {}\n", render_status(status))),
        Err(e) => Response::text(503, "Service Unavailable", format!("{e}\n")),
    }
}

/// The plain-text rendering of a [`RoomStatus`] (OPS §3: human-readable,
/// no JSON).
fn render_status(status: RoomStatus) -> String {
    match status {
        RoomStatus::Running { members } => format!("running members={members}"),
        RoomStatus::Destroyed => "destroyed".to_owned(),
        RoomStatus::Absent => "absent".to_owned(),
    }
}

/// First occurrence of `key` in the query string, hand-rolled (OPS
/// decision 3): pairs split on `&`, key/value on the FIRST `=`. Values are
/// numeric everywhere they are used, so percent-decoding is unnecessary.
fn query_value<'q>(query: &'q str, key: &str) -> Option<&'q str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Required positive-integer parameter (`id`). An empty or non-numeric or
/// zero value is a bad request, not a default.
fn required_id(query: &str) -> Option<u64> {
    query_value(query, "id")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&id| id > 0)
}

/// Optional finite-positive float parameter (`tick_hz`).
fn optional_f64(query: &str, key: &str) -> Result<Option<f64>, &'static str> {
    match query_value(query, key) {
        None => Ok(None),
        Some(v) => match v.parse::<f64>() {
            Ok(x) if x.is_finite() && x > 0.0 => Ok(Some(x)),
            _ => Err("`tick_hz` must be a finite positive number\n"),
        },
    }
}

/// The bookkeeper: owns the set of room ids this surface lists. Task-local
/// state mutated by messages — the same ownership discipline as every other
/// actor in this codebase (no shared cells anywhere on this surface).
///
/// v1 simplification (documented): the set is the CONFIGURED rooms plus
/// every room opened THROUGH THIS SURFACE. Rooms opened programmatically
/// via `ServerHandle::open_room` (or the wire protocol, were one to exist)
/// do not appear until the process restarts with them configured — their
/// statuses are still reachable through `/metrics`, and the set is a
/// listing convenience, not an authority (the registry stays the single
/// authority on room existence; every rendered status is queried there).
async fn run_room_bookkeeper(mut rx: mpsc::Receiver<RoomsMsg>, mut known: BTreeSet<u64>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            RoomsMsg::Seen(id) => {
                known.insert(id.0);
            }
            RoomsMsg::Listing(reply) => {
                let _ = reply.send(known.iter().copied().collect());
            }
        }
    }
}
