//! `ServerHandle::stop`: the shutdown cascade's entry point (DESIGN §9),
//! the accept loops' graceful end (BACKLOG B16; the ops HTTP surface's
//! too since B33), and the game services' explicit stop after the rooms
//! (BACKLOG F5, DESIGN §9.2).

use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::warn;

use gsb_core::registry::RegistryMsg;
use gsb_core::service::{Released, Service};

use super::ServerHandle;

/// How long `stop` waits for the accept loops to end on their closed
/// listeners before it aborts the stragglers. Every in-tree listener
/// ends its loop in well under a millisecond (its `close` ends the
/// pending accept); the grace exists for a listener that does not, and
/// it bounds the time such a listener adds to `stop`.
pub(crate) const ACCEPT_STOP_GRACE: Duration = Duration::from_secs(1);

/// How long `stop` waits for the rooms to finish their teardown, and then
/// — a second window of the same length — for the asked services to end.
/// In-tree rooms end within a tick of the stop and the demo's economy
/// service within its request latency; the grace bounds what a stuck room
/// or a service that ignores its stop request adds to `stop`.
pub(crate) const SERVICE_STOP_GRACE: Duration = Duration::from_secs(1);

/// What `stop` did with the accept loops, the rooms and the game's
/// services. Observability only: `stop` completes either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StopReport {
    /// Accept loops that ended by themselves on their closed listener —
    /// one per game listener, plus the HTTP ops surface's when
    /// `http_listen` is set (its door closes with the listeners', B33).
    pub accept_loops_ended: usize,
    /// Accept loops still running after the one-second stop grace and
    /// aborted (a listener whose `close` does not end its accept). Zero
    /// with the in-tree transports and the ops surface.
    pub accept_loops_aborted: usize,
    /// Whether every room and shard task had ended — its `on_shutdown` and
    /// `match_result` run — within the stop grace. `false` = the services
    /// were asked to stop anyway (a room's late messages may miss them).
    pub rooms_finished: bool,
    /// Registered services that ended after their stop request.
    pub services_ended: usize,
    /// Registered services still running at the stop grace and aborted
    /// (one that ignores its stop request). Zero with the in-tree games.
    pub services_aborted: usize,
    /// Whether the metrics collector's final report is complete: every
    /// session producer — the registry, every room, shard and dispatcher,
    /// every connection actor — had ended, its last word folded in
    /// (BACKLOG F35). `false` = the report went out at the collector's
    /// grace (`gsb_core::metrics::FINAL_REPORT_GRACE`) with a producer
    /// still running, without that producer's last word.
    pub final_report_complete: bool,
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
    /// The HTTP ops surface's door closes first and its accept loop is
    /// waited for with the listeners' (BACKLOG B33). Then the game's registered
    /// services stop, AFTER the rooms (BACKLOG F5): `stop` waits (bounded)
    /// until every room and shard task has run its teardown, asks each
    /// service to stop, and waits for them under one deadline, aborting a
    /// straggler. The metrics collector is awaited
    /// last: once the broadcast has closed it folds every session
    /// producer's last word — each room's final count, each connection's
    /// final flush — and emits its final report when all of them have
    /// ended (BACKLOG F35), or at its own grace
    /// (`gsb_core::metrics::FINAL_REPORT_GRACE`, from the close, running
    /// alongside the waits above) if one has not
    /// ([`StopReport::final_report_complete`]). The
    /// teardown ORDER is the single-listener order applied across all
    /// listeners: doors close first, so no new client can connect while
    /// the registry is tearing the existing ones down.
    pub async fn stop(self) -> StopReport {
        let _ = self.registry.send(RegistryMsg::Shutdown).await;
        let mut accepts = self.accepts;
        if let Some(http) = self.http {
            http.door.close();
            accepts.push(http.task);
        }
        self.ticker.abort();
        for l in &self.listeners {
            l.close();
        }
        let mut report = end_accepts(accepts, ACCEPT_STOP_GRACE).await;
        end_services(
            self.services,
            self.rooms_released,
            SERVICE_STOP_GRACE,
            &mut report,
        )
        .await;
        // Bounded by the collector's own grace once the broadcast has
        // closed (the registry, which holds the last `Ticker`, exits on
        // the Shutdown above without awaiting any room — S rule).
        report.final_report_complete = self.metrics.await.unwrap_or(false);
        report
    }
}

/// Wait for every accept loop to end, all under ONE deadline (`grace`
/// from now, not per loop); abort whichever is still running at it.
/// Each wait is a single awaited join under a deadline — the pump
/// idiom — so `stop` still always completes.
pub(crate) async fn end_accepts(accepts: Vec<JoinHandle<()>>, grace: Duration) -> StopReport {
    let deadline = tokio::time::Instant::now() + grace;
    let mut report = StopReport::default();
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

/// Stop the game's services after the rooms: wait for the rooms' drop
/// barrier (the registry and every room and shard task ended) up to
/// `grace`, THEN ask every service to stop — synchronous requests, so
/// what the rooms queued on their way out is ahead of each stop message —
/// and wait for all of them under ONE further `grace` deadline, aborting
/// whichever is still running at it. Two single awaits under deadlines,
/// like `end_accepts`: nothing here can hold `stop` longer than twice the
/// grace.
pub(crate) async fn end_services(
    services: Vec<Service>,
    rooms: Released,
    grace: Duration,
    report: &mut StopReport,
) {
    report.rooms_finished = tokio::time::timeout(grace, rooms.wait()).await.is_ok();
    if !report.rooms_finished {
        warn!(
            ?grace,
            "rooms still tearing down at the stop grace; stopping the services anyway"
        );
    }
    let asked: Vec<_> = services
        .into_iter()
        .map(|s| (s.name(), s.request_stop()))
        .collect();
    let deadline = tokio::time::Instant::now() + grace;
    for (name, mut task) in asked {
        match tokio::time::timeout_at(deadline, &mut task).await {
            Ok(_) => report.services_ended += 1,
            Err(_) => {
                task.abort();
                report.services_aborted += 1;
                warn!(
                    service = name,
                    ?grace,
                    "service still running after its stop request; aborted"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
