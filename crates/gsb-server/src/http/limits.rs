//! The ops surface's resource limits: a cap on its concurrent
//! connections and a deadline on writing each response (BACKLOG B49), and
//! a deadline on routing each request (B90).
//!
//! B47 bounded the head read, but a peer that sent its head and never
//! read a response larger than the socket buffers (a many-room
//! `/metrics`) held its task for as long as it stayed connected, and
//! nothing bounded how many such tasks a peer could open. Now:
//!
//! - **The cap.** The accept loop holds a slot per live connection task;
//!   over the cap a new connection is closed at once, unread and
//!   unanswered — no write on the refusal path (a slow reader would make
//!   it work) — and counted (`ops_http_conns_refused`); the first refusal
//!   of a saturated spell warns, the rest are `debug`.
//! - **The deadline.** The response write (and the half-close after it)
//!   runs under ONE timeout for the whole response, like B47's head read;
//!   past it the connection is dropped (closed) and counted
//!   (`ops_http_writes_timed_out`).
//! - **The routing deadline (B90).** `/rooms` and the room open/close
//!   wait for the room bookkeeper's and the registry's answers; a stalled
//!   registry held the task with them. The whole routing step runs under
//!   ONE timeout too; past it the request is answered `504` and counted
//!   (`ops_http_routes_timed_out`; see [`route_in_time`]). A task
//!   therefore lives at most `HEAD_DEADLINE` + the routing deadline + the
//!   write deadline + `DRAIN_WINDOW`.
//! - **The count.** The connection tasks and the accept loop share the
//!   counters (atomics; no lock); the accept loop — the surface's one
//!   long-lived task — sends their growth to the collector's transport
//!   channel (`gsb_net::Flusher`, the doors' rule: at most every 500 ms
//!   after an accept, and once more when the door closes). A write
//!   or routing timeout after the last accept is reported at the next
//!   accept or at the close — the handshake intake's rule (B58).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use gsb_core::metrics::TransportCounters;

use tracing::warn;

use crate::config::Config;
use crate::http::OpsHttp;
use crate::http::response::Response;

/// What the ops surface is spawned with for B49: its limits, and where
/// it counts (the collector's transport channel; `None` = nowhere).
pub(crate) struct OpsGuard {
    pub(crate) limits: OpsLimits,
    pub(crate) metrics: gsb_net::TransportMetrics,
}

/// The ops surface's limits, resolved from the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpsLimits {
    /// Concurrent connections (`None` = no cap).
    pub(crate) max_connections: Option<usize>,
    /// The whole response write's deadline (`None` = none).
    pub(crate) write_timeout: Option<Duration>,
    /// The whole routing step's deadline (B90; `None` = none).
    pub(crate) route_timeout: Option<Duration>,
}

impl OpsLimits {
    /// `http_max_connections` (`0` or unset = no cap),
    /// `http_write_timeout_secs` and `http_route_timeout_secs` (`0`,
    /// negative or not finite = none — the `write_stall_secs` rule).
    pub(crate) fn of(cfg: &Config) -> Self {
        Self {
            max_connections: cfg
                .http_max_connections
                .filter(|&n| n > 0)
                .map(|n| n as usize),
            write_timeout: secs(cfg.http_write_timeout_secs),
            route_timeout: secs(cfg.http_route_timeout_secs),
        }
    }
}

/// Route one request under the routing deadline (B90): one timeout
/// around the whole step — the bookkeeper's and the registry's answers,
/// the wait for a slot in a full registry mailbox included. Past it the
/// wait is dropped (the reply, if one still comes, finds nobody) and the
/// request gets a `504`: the registry was asked and did not answer in
/// time, so an open or close may still take effect — `503` would say it
/// was not done. Counted, and warned per request (operator-paced).
pub(crate) async fn route_in_time(head: &str, ops: &OpsHttp) -> Response {
    let routing = super::routes::route(head, ops);
    let Some(deadline) = ops.limits.route_timeout else {
        return routing.await;
    };
    match tokio::time::timeout(deadline, routing).await {
        Ok(response) => response,
        Err(_) => {
            ops.counters.count_route_timeout();
            warn!(timeout = ?deadline, "ops http request not routed in time; the registry did not answer");
            Response::text(
                504,
                "Gateway Timeout",
                format!(
                    "the registry did not answer within {:.3}s; an open or close may still take effect (check GET /rooms)\n",
                    deadline.as_secs_f64()
                ),
            )
        }
    }
}

/// A deadline in seconds: `0`, negative or not finite = none.
fn secs(s: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(s).ok().filter(|d| !d.is_zero())
}

/// What the accept loop and its connection tasks count together.
#[derive(Debug, Default)]
pub(crate) struct OpsCounters {
    live: AtomicUsize,
    refused: AtomicU64,
    writes_timed_out: AtomicU64,
    routes_timed_out: AtomicU64,
}

/// A live connection's place under the cap; given back on drop.
pub(crate) struct ConnSlot(Arc<OpsCounters>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}

impl OpsCounters {
    /// A slot for a new connection under `max`, or its refusal counted.
    pub(crate) fn try_slot(self: &Arc<Self>, max: Option<usize>) -> Option<ConnSlot> {
        let cap = max.unwrap_or(usize::MAX);
        let taken = self
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < cap).then_some(n + 1)
            })
            .is_ok();
        if !taken {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(ConnSlot(Arc::clone(self)))
    }

    /// Count a response write cut at its deadline.
    pub(crate) fn count_write_timeout(&self) {
        self.writes_timed_out.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a request whose routing outran its deadline (B90).
    pub(crate) fn count_route_timeout(&self) {
        self.routes_timed_out.fetch_add(1, Ordering::Relaxed);
    }

    /// Live connection tasks now.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// The totals, as the transport slice of the report carries them.
    pub(crate) fn totals(&self) -> TransportCounters {
        TransportCounters {
            ops_http_conns_refused: self.refused.load(Ordering::Relaxed),
            ops_http_writes_timed_out: self.writes_timed_out.load(Ordering::Relaxed),
            ops_http_routes_timed_out: self.routes_timed_out.load(Ordering::Relaxed),
            ..Default::default()
        }
    }
}
