//! The export seam: where a folded [`MetricReport`] leaves the server.
//!
//! Collection stays what it was — actor-local counters, samples over a
//! bounded channel, one collector folding them (the parent's docs). The
//! collector's `emit` is the ONE place a report is handed on: first to
//! every [`Exporter`] (in the order they were given), then to the
//! collector's [`MetricSink`](crate::metrics::MetricSink) (log lines, a programmatic channel, or the
//! ops surface's latest-report `watch`).
//!
//! Two directions, one rule each:
//!
//! - **Pull** (Prometheus scrape): the scraper asks, so the exporter is a
//!   pure function of the latest report — the ops surface keeps it in its
//!   `watch` snapshot and renders at scrape time
//!   (`MetricReport::render_prometheus`, feature `prometheus`). Nothing is
//!   rendered while nobody scrapes.
//! - **Push** (OTLP, feature `otlp`): an [`Exporter`] called at the
//!   collector's cadence. It must return without blocking — the collector
//!   is single-awaited and drains the samples of every actor; an exporter
//!   that does I/O hands the report to its own task over a bounded channel
//!   with `try_send` and counts what a full channel drops (every value is
//!   cumulative or a gauge, so the next hand-off carries what a dropped
//!   one would have).

use crate::metrics::MetricReport;

#[cfg(any(feature = "prometheus", feature = "otlp"))]
mod families;
#[cfg(feature = "otlp")]
pub mod otlp;
#[cfg(feature = "prometheus")]
mod prometheus;

/// A consumer of every report the collector folds.
///
/// Called synchronously from the collector's task, once per report
/// (including the final one at shutdown), BEFORE the collector's sink
/// consumes it. An implementation must not block or park: a slow
/// exporter would stall the drain of every actor's samples. I/O belongs
/// in the exporter's own task, fed by a bounded `try_send`.
///
/// Exporters are pure consumers: they read the report, they never reach
/// back into the collector or the actors (there is no handle to reach
/// with), and no actor knows how many exporters exist.
pub trait Exporter: Send + 'static {
    /// Take one report (read-only; clone what must outlive the call).
    fn export(&mut self, report: &MetricReport);
}

impl<F> Exporter for F
where
    F: FnMut(&MetricReport) + Send + 'static,
{
    fn export(&mut self, report: &MetricReport) {
        self(report)
    }
}
