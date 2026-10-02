//! The ops surface's two resource limits (BACKLOG B49): a cap on its
//! concurrent connections and a deadline on writing each response.
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
//!   (`ops_http_writes_timed_out`). A task therefore lives at most
//!   `HEAD_DEADLINE` + routing + the write deadline + `DRAIN_WINDOW`.
//! - **The count.** The connection tasks and the accept loop share the
//!   counters (atomics; no lock); the accept loop — the surface's one
//!   long-lived task — sends their growth to the collector's transport
//!   channel (`gsb_net::Flusher`, the doors' rule: at most every 500 ms
//!   after an accept, and once more when the door closes). A write
//!   timeout after the last accept is reported at the next accept or at
//!   the close — the handshake intake's rule (B58).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use gsb_core::metrics::TransportCounters;

use crate::config::Config;

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
}

impl OpsLimits {
    /// `http_max_connections` (`0` or unset = no cap) and
    /// `http_write_timeout_secs` (`0`, negative or not finite = none —
    /// the `write_stall_secs` rule).
    pub(crate) fn of(cfg: &Config) -> Self {
        Self {
            max_connections: cfg
                .http_max_connections
                .filter(|&n| n > 0)
                .map(|n| n as usize),
            write_timeout: Duration::try_from_secs_f64(cfg.http_write_timeout_secs)
                .ok()
                .filter(|d| !d.is_zero()),
        }
    }
}

/// What the accept loop and its connection tasks count together.
#[derive(Debug, Default)]
pub(crate) struct OpsCounters {
    live: AtomicUsize,
    refused: AtomicU64,
    writes_timed_out: AtomicU64,
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
            ..Default::default()
        }
    }
}
