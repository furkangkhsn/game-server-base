//! The collector's end (BACKLOG F35): the final report waits for the
//! producers' last words.
//!
//! The ticker's close is the server's stop signal, and the rooms and the
//! connection actors only START their ends on it: a room sees the same
//! close (or its control `Shutdown`) and then runs its teardown hooks,
//! counts what it still held and hands the collector its final sample
//! (`RoomFinal`, B62); a connection actor gets its `Shutdown` from the
//! registry's teardown and sends its final flush. Emitting the final
//! report on the close itself raced all of them — a room that had not
//! reached its first periodic sample (one per metrics period) was then
//! missing from the report altogether, and every room lost what it
//! counted since its last sample.
//!
//! So after the close the collector awaits its event channel — the one
//! source it still has — folding every event, until the channel CLOSES:
//! every sender dropped, i.e. every producer ended. A producer's last
//! word is sent before it ends, and one delivered by a spawned sender
//! (`crate::channel::post`, when the channel was full) holds its own
//! sender clone until it is in: the close cannot overtake either. Then
//! the final report goes out.
//!
//! The wait is bounded by [`FINAL_REPORT_GRACE`] from the close: a
//! producer that outlives the stop (a stuck room, a connection accepted
//! after the registry's teardown) cannot hold the final report — it goes
//! out at the grace without that producer's last word, with a `warn`, and
//! [`super::MetricsCollector::run`] says so. Transport tasks may send on
//! a channel of their own ([`super::MetricsCollector::with_transport_events`])
//! whose close is not waited for: they end with their sockets.

use std::time::Duration;

use super::MetricsCollector;

/// How long the collector's final report waits, from the ticker's close,
/// for every producer to drop its sender (see the module docs). In the
/// server's stop the producers end within the stop cascade: the rooms on
/// the close itself, the connection actors on the registry's teardown,
/// the accept loops (which hand each new connection actor its sender) by
/// the stop's one-second accept grace at the latest. Two seconds
/// outlasts that backstop, and the stop's own bounds (the accept grace,
/// then the rooms' and the services' graces) already allow three: the
/// collector's wait runs alongside them and adds nothing in practice.
pub const FINAL_REPORT_GRACE: Duration = Duration::from_secs(2);

impl MetricsCollector {
    /// Fold events until every producer has dropped its sender (`true`)
    /// or the grace passed (`false`). One awaited source — the event
    /// channel's receive, under one deadline (the pump idiom) — and the
    /// transport channel drained after each event, synchronously.
    pub(super) async fn fold_last_words(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + self.final_grace;
        let complete = loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(ev)) => {
                    self.acc.apply(ev);
                    self.drain_transport();
                }
                Ok(None) => break true,
                Err(_) => {
                    tracing::warn!(
                        grace = ?self.final_grace,
                        "metrics producers still running at the final-report grace; \
                         the final report goes out without their last words"
                    );
                    break false;
                }
            }
        };
        self.drain_transport();
        complete
    }
}
