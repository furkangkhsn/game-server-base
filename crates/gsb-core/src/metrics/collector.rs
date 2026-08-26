//! The collector task: one owner of the accumulator, one awaited
//! source (its sample mailbox), publishing on a period.

use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};

use crate::ticker::TickInfo;
use crate::metrics::*;


/// Where reports go. All sinks are message-passing: the collector never
/// shares its accumulator.
pub enum MetricSink {
    /// One `tracing::info!` line per rendered report line (visible under
    /// `RUST_LOG=info`; silent where no subscriber is installed, e.g. in
    /// most tests).
    Log,
    /// Send each [`MetricReport`] to a channel (programmatic consumers:
    /// the load generator, tests).
    Channel(mpsc::UnboundedSender<MetricReport>),
    /// Publish the latest report through a `tokio::sync::watch` channel
    /// (the HTTP ops surface's scrape snapshot). WHY watch and not another
    /// mpsc: the consumer wants *latest*, never a queue — overwrite
    /// semantics mean a scraper that reads between periods always sees the
    /// newest report and a slow reader can never back up the collector.
    /// The send is synchronous (no await in the collector's tick body) and
    /// a closed receiver (HTTP surface shut down or never attached) fails
    /// harmlessly — the same "consumer gone" tolerance as [`MetricSink::Log`]
    /// without a subscriber.
    Watch(watch::Sender<MetricReport>),
}

/// The metrics collector task.
///
/// One awaited source (a subscription to the global ticker's broadcast —
/// the same channel the rooms use), no `select!`, no shared state: on
/// each tick it drains the event channel with `try_recv` (synchronous)
/// and, at most once per `period`, emits a report through its sink. When
/// the ticker closes (shutdown), it emits one final report and exits.
pub struct MetricsCollector {
    ticks: broadcast::Receiver<TickInfo>,
    /// Bounded (see module docs: bounded + `try_send` producers with a drop
    /// counter); the collector drains it with `try_recv` on every tick.
    rx: mpsc::Receiver<MetricsEvent>,
    acc: MetricAccumulator,
    sink: MetricSink,
    period: Duration,
    next_report: Instant,
}

impl MetricsCollector {
    pub fn new(
        ticks: broadcast::Receiver<TickInfo>,
        rx: mpsc::Receiver<MetricsEvent>,
        sink: MetricSink,
        period: Duration,
    ) -> Self {
        Self {
            ticks,
            rx,
            acc: MetricAccumulator::default(),
            sink,
            period,
            next_report: Instant::now() + period,
        }
    }

    /// Run until the ticker closes (one final report is emitted).
    pub async fn run(mut self) {
        loop {
            // The collector's only awaited source: the ticker broadcast.
            // `Lagged` is irrelevant here (we do not index ticks); we just
            // resynchronize with the next one.
            let closed = matches!(
                self.ticks.recv().await,
                Err(broadcast::error::RecvError::Closed)
            );
            let now = Instant::now();
            while let Ok(ev) = self.rx.try_recv() {
                self.acc.apply(ev);
            }
            if now >= self.next_report {
                self.emit(now);
                self.next_report = now + self.period;
            }
            if closed {
                break;
            }
        }
        self.emit(Instant::now());
    }

    fn emit(&mut self, at: Instant) {
        let report = self.acc.report(at);
        match &self.sink {
            MetricSink::Log => {
                for line in report.render() {
                    tracing::info!(%line, "gsb-metric");
                }
            }
            MetricSink::Channel(tx) => {
                // Synchronous send: the collector is a plain task, not an
                // actor under the single-`await` discipline; a closed
                // receiver (consumer gone) is ignored.
                let _ = tx.send(report);
            }
            MetricSink::Watch(tx) => {
                // Synchronous overwrite ("latest wins" — see the variant
                // docs); a closed receiver is ignored like a dropped log.
                let _ = tx.send(report);
            }
        }
    }
}
