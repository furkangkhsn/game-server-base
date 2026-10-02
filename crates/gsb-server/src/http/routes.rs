//! Routing and the handlers behind each endpoint.

use crate::boot::{registry_close_room, registry_open_room, registry_room_status};
use crate::http::response::Response;
use crate::http::*;
use gsb_core::error::CoreError;
use gsb_core::id::RoomId;
use gsb_core::registry::RoomStatus;
use std::collections::BTreeSet;
use tokio::sync::{mpsc, oneshot};

/// Route one parsed request head. Known paths with the wrong verb answer
/// 405 (the resource exists, the method does not); unknown paths answer
/// 404; anything unparseable never gets here (`read_head` said 400).
pub(super) async fn route(head: &str, ops: &OpsHttp) -> Response {
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
pub(super) fn healthz(ops: &OpsHttp) -> Response {
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
/// latest borrowed snapshot — the pull exporter (`gsb_core::metrics`'s
/// export seam), rendered at scrape time. Rendering is synchronous over
/// the borrow — no await between `borrow()` and the last read, so the
/// guard can never leak across a wait point.
#[cfg(feature = "prometheus")]
pub(super) fn metrics_snapshot(ops: &OpsHttp) -> Response {
    let body = ops.reports.borrow().render_prometheus();
    Response {
        status: 200,
        reason: "OK",
        content_type: "text/plain; version=0.0.4",
        allow: None,
        body,
    }
}

/// GET /metrics in a build without the Prometheus exporter: the path is
/// known (405 for another verb), the exposition is not compiled in — a
/// 404 that says why instead of an empty 200 a scraper would ingest.
#[cfg(not(feature = "prometheus"))]
pub(super) fn metrics_snapshot(_ops: &OpsHttp) -> Response {
    Response::text(
        404,
        "Not Found",
        "this build has no Prometheus exporter (cargo feature `prometheus`)\n",
    )
}

/// GET /rooms: the known room ids (the configured ones plus everything
/// opened through this surface) with their CURRENT registry statuses. The
/// ids come from the bookkeeper; the statuses come from the registry — so
/// a room closed elsewhere shows up as absent here too.
pub(super) async fn list_rooms(ops: &OpsHttp) -> Response {
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
/// as a status query). The room is the SERVER's room of that id (the
/// template every pre-created room comes from, with the id's
/// `[rooms.<id>]` laid over it — `Config::room_config`), so re-opening a
/// pre-created room with the same rate is the idempotent no-op; `tick_hz`
/// is the one per-request override, on top of the id's override
/// (omitted = the room's rate: the override's, else the server's).
pub(super) async fn open_room(query: &str, ops: &OpsHttp) -> Response {
    let Some(id) = required_id(query) else {
        return Response::text(
            400,
            "Bad Request",
            "missing or invalid `id` (expected a positive integer)\n",
        );
    };
    let mut config = ops.room_template.room(id);
    match optional_f64(query, "tick_hz") {
        Ok(Some(tick_hz)) => config.tick_hz = tick_hz,
        Ok(None) => {}
        Err(msg) => return Response::text(400, "Bad Request", msg),
    }
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
pub(super) async fn close_room(query: &str, ops: &OpsHttp) -> Response {
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
pub(super) fn render_status(status: RoomStatus) -> String {
    match status {
        RoomStatus::Running { members } => format!("running members={members}"),
        RoomStatus::Destroyed => "destroyed".to_owned(),
        RoomStatus::Absent => "absent".to_owned(),
    }
}

/// First occurrence of `key` in the query string, hand-rolled (OPS
/// decision 3): pairs split on `&`, key/value on the FIRST `=`. Values are
/// numeric everywhere they are used, so percent-decoding is unnecessary.
pub(super) fn query_value<'q>(query: &'q str, key: &str) -> Option<&'q str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Required positive-integer parameter (`id`). An empty or non-numeric or
/// zero value is a bad request, not a default.
pub(super) fn required_id(query: &str) -> Option<u64> {
    query_value(query, "id")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&id| id > 0)
}

/// Optional finite-positive float parameter (`tick_hz`).
pub(super) fn optional_f64(query: &str, key: &str) -> Result<Option<f64>, &'static str> {
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
pub(super) async fn run_room_bookkeeper(
    mut rx: mpsc::Receiver<RoomsMsg>,
    mut known: BTreeSet<u64>,
) {
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
