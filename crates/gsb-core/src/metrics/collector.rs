//! The collector task: one owner of the accumulator, one awaited
//! source at a time (the ticker while the server runs, then its event
//! channel while it stops — `closing`), publishing on a period — a
//! period's report waiting, bounded, for a sharded room's round in
//! flight to land (`cut`).

use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};

use crate::metrics::*;
use crate::ticker::TickInfo;

mod closing;
pub use closing::FINAL_REPORT_GRACE;
mod cut;
pub use cut::CUT_GRACE;

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
/// One awaited source at a time, no multiplexing, no shared state. While
/// the server runs, that source is a subscription to the global ticker's
/// broadcast (the same channel the rooms use): on each tick it drains its
/// event channels with `try_recv` (synchronous) and, at most once per
/// `period`, emits a report through its sink — on the first tick after
/// the report fell due that finds no sharded room's round in flight, or
/// at [`CUT_GRACE`] (BACKLOG F29, see `cut`). The schedule runs on the
/// tick clock (`crate::ticker::now`: the wall clock in production, the
/// virtual one under a paused test runtime). When the ticker closes
/// (shutdown) the rooms and connections are only starting their own ends,
/// so the collector then awaits its event channel instead, folding every
/// last word (a room's `RoomFinal`, a connection's final sample) until
/// every producer has dropped its sender — or [`FINAL_REPORT_GRACE`]
/// passed — and only then emits its final report and exits (BACKLOG F35,
/// see `closing`).
pub struct MetricsCollector {
    ticks: broadcast::Receiver<TickInfo>,
    /// Bounded (see module docs: bounded + `try_send` producers with a drop
    /// counter); the collector drains it with `try_recv` on every tick,
    /// and its CLOSE — every sender dropped — is what the final report
    /// waits for (see `closing`).
    rx: mpsc::Receiver<MetricsEvent>,
    /// The transport tasks' own channel, when they have one of their own
    /// ([`Self::with_transport_events`]): drained like `rx`, but its close
    /// is not waited for — a transport task can outlive the server's stop
    /// by as long as its peer keeps the socket open.
    transport: Option<mpsc::Receiver<MetricsEvent>>,
    acc: MetricAccumulator,
    sink: MetricSink,
    /// The push-side consumers (see `export`): each sees every report,
    /// in this order, before the sink consumes it.
    exporters: Vec<Box<dyn Exporter>>,
    period: Duration,
    /// When the next periodic report falls due, on the tick clock.
    next_report: Instant,
    /// How long a due report waits, at most, for a sharded room's round
    /// in flight (see `cut`).
    cut_grace: Duration,
    /// How long the final report waits for the producers after the ticker
    /// closed (see `closing`).
    final_grace: Duration,
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
            transport: None,
            acc: MetricAccumulator::default(),
            sink,
            exporters: Vec::new(),
            period,
            next_report: crate::ticker::now() + period,
            cut_grace: CUT_GRACE,
            final_grace: FINAL_REPORT_GRACE,
        }
    }

    /// Take the transport tasks' events (the rUDP demux and writers, the
    /// stream pumps, the handshake intakes — `gsb_net`'s
    /// `TransportMetrics`) from a channel of their own. They are folded
    /// like any event, but the final report does not wait for that
    /// channel to close: a transport task ends with its socket, which a
    /// silent peer can hold open past the server's stop. Without it the
    /// transports send on the main channel and the final report waits
    /// for them too.
    pub fn with_transport_events(mut self, rx: mpsc::Receiver<MetricsEvent>) -> Self {
        self.transport = Some(rx);
        self
    }

    /// Bound the final report's wait for the producers (default
    /// [`FINAL_REPORT_GRACE`]; see `closing`).
    pub fn with_final_grace(mut self, grace: Duration) -> Self {
        self.final_grace = grace;
        self
    }

    /// Bound a due report's wait for a sharded room's round in flight
    /// (default [`CUT_GRACE`], capped at half the period; `Duration::ZERO`
    /// never waits; see `cut`).
    pub fn with_cut_grace(mut self, grace: Duration) -> Self {
        self.cut_grace = grace;
        self
    }

    /// Hand every report to `exporters` as well (in this order, before
    /// the sink). The collector stays the one place export happens; an
    /// exporter only ever sees the folded report (see [`Exporter`]).
    pub fn with_exporters(mut self, exporters: Vec<Box<dyn Exporter>>) -> Self {
        self.exporters.extend(exporters);
        self
    }

    /// Run until the ticker closes, then fold the producers' last words
    /// and emit one final report (see `closing`). Returns whether that
    /// report is complete: `true` when every producer had dropped its
    /// sender, `false` when it went out at the grace with one still
    /// holding it (a `warn` names the grace).
    pub async fn run(mut self) -> bool {
        loop {
            // The collector's only awaited source: the ticker broadcast.
            // `Lagged` is irrelevant here (we do not index ticks); we just
            // resynchronize with the next one.
            let closed = matches!(
                self.ticks.recv().await,
                Err(broadcast::error::RecvError::Closed)
            );
            let now = crate::ticker::now();
            while let Ok(ev) = self.rx.try_recv() {
                self.acc.apply(ev);
            }
            self.drain_transport();
            if now >= self.next_report && self.ready_to_emit(now) {
                self.emit(Instant::now());
                self.next_report = now + self.period;
            }
            if closed {
                break;
            }
        }
        let complete = self.fold_last_words().await;
        self.emit(Instant::now());
        complete
    }

    /// Fold whatever the transport channel holds (synchronous).
    fn drain_transport(&mut self) {
        if let Some(rx) = &mut self.transport {
            while let Ok(ev) = rx.try_recv() {
                self.acc.apply(ev);
            }
        }
    }

    fn emit(&mut self, at: Instant) {
        let report = self.acc.report(at);
        // The export seam (see `export`): every exporter reads the
        // report first, synchronously and in order; the sink consumes it.
        for exporter in &mut self.exporters {
            exporter.export(&report);
        }
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
