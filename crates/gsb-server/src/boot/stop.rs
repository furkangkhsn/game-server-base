//! `ServerHandle::stop`: the shutdown cascade's entry point (DESIGN §9),
//! and the accept loops' graceful end (BACKLOG B16).

use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::warn;

use gsb_core::registry::RegistryMsg;

use super::ServerHandle;

/// How long `stop` waits for the accept loops to end on their closed
/// listeners before it aborts the stragglers. Every in-tree listener
/// ends its loop in well under a millisecond (its `close` ends the
/// pending accept); the grace exists for a listener that does not, and
/// it bounds the time such a listener adds to `stop`.
pub(crate) const ACCEPT_STOP_GRACE: Duration = Duration::from_secs(1);

/// What `stop` did with the accept loops. Observability only: `stop`
/// completes either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopReport {
    /// Accept loops that ended by themselves on their closed listener.
    pub accept_loops_ended: usize,
    /// Accept loops still running after the one-second stop grace and
    /// aborted (a listener whose `close` does not end its accept). Zero
    /// with the in-tree transports.
    pub accept_loops_aborted: usize,
}

impl ServerHandle {
    /// Shut the server down: the registry tears down connections and rooms
    /// (rooms get a control `Shutdown`, processed on their next tick; a
    /// room with a configured result seam reports it on that shutdown); the
    /// ticker is aborted, which closes the broadcast and stops any room that
    /// missed its window; EVERY listener is closed (stopping any per-
    /// listener transport shared state, e.g. each rUDP demux), which ends
    /// its accept loop: the pending accept returns the listener-closed
    /// error and the loop returns (BACKLOG B16). `stop` waits for the loops
    /// up to one second in all and aborts only one that overran it.
    /// The HTTP ops surface is aborted. The metrics collector is awaited
    /// last: it emits one final report when the broadcast closes. The
    /// teardown ORDER is the single-listener order applied across all
    /// listeners: doors close first, so no new client can connect while
    /// the registry is tearing the existing ones down.
    pub async fn stop(self) -> StopReport {
        let _ = self.registry.send(RegistryMsg::Shutdown).await;
        if let Some(http) = self.http {
            http.abort();
        }
        self.ticker.abort();
        for l in &self.listeners {
            l.close();
        }
        let report = end_accepts(self.accepts, ACCEPT_STOP_GRACE).await;
        let _ = self.metrics.await;
        report
    }
}

/// Wait for every accept loop to end, all under ONE deadline (`grace`
/// from now, not per loop); abort whichever is still running at it.
/// Each wait is a single awaited join under a deadline — the pump
/// idiom — so `stop` still always completes.
pub(crate) async fn end_accepts(accepts: Vec<JoinHandle<()>>, grace: Duration) -> StopReport {
    let deadline = tokio::time::Instant::now() + grace;
    let mut report = StopReport {
        accept_loops_ended: 0,
        accept_loops_aborted: 0,
    };
    for mut accept in accepts {
        match tokio::time::timeout_at(deadline, &mut accept).await {
            Ok(_) => report.accept_loops_ended += 1,
            Err(_) => {
                accept.abort();
                report.accept_loops_aborted += 1;
            }
        }
    }
    if report.accept_loops_aborted > 0 {
        warn!(
            aborted = report.accept_loops_aborted,
            ?grace,
            "accept loops still running after their listeners closed; aborted"
        );
    }
    report
}

#[cfg(test)]
mod tests;
