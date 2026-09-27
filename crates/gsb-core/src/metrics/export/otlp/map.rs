//! A folded [`MetricReport`] as one OTLP export request: the shared
//! family table (`families`) walked in its order, each family mapped by
//! its type.
//!
//! | Our family | OTLP |
//! |---|---|
//! | counter (cumulative `u64`) | `Sum`, monotonic, CUMULATIVE, `as_int` |
//! | gauge (registry counts) | `Gauge`, `as_int` |
//! | gauge (per-room: rates, means, extremes, counts) | `Gauge`, `as_double` |
//! | log2 step histogram | `Histogram`, CUMULATIVE, bounds = the room's budget edges in µs |
//! | fine step histogram | `Histogram`, CUMULATIVE, bounds 8, 16, …, 4096 µs; the last bucket holds the steps past the fine cap |
//! | logic SUM / MAX | `Sum` monotonic / `Gauge`, `as_int` |
//!
//! Names are the exposition's minus `_total` (a collector's Prometheus
//! exporter appends it again to a monotonic sum, so both surfaces meet
//! in the same series names). The room is an attribute (`room="r<id>"`,
//! the label's value), the close reason too (`reason`). Units stay in
//! the names (`_us`, `_bytes`), the `unit` field empty: a unit there
//! would be appended to the name a second time by that same exporter.

use super::proto::{self, metric::Data};
use crate::metrics::export::families::{
    self, Kind, LOGIC_DROPPED, LOGIC_MAX_HELP, LOGIC_SUM_HELP, RoomValue, SERVER_CLOSES,
};
use crate::metrics::*;

mod build;
use build::{attr, double_point, gauge, int_point, metric, room, scalar, sum};

/// The instrumentation scope's name (the version is the crate's).
pub(super) const SCOPE: &str = "gsb";

/// The fine histogram's description: here it is a histogram, not the
/// exposition's p50/p99 summary.
const FINE_HELP: &str = "Step body duration, µs, in fixed 8 µs buckets; the last bucket holds the steps at/above the fine cap (4096 µs).";

/// When the values were taken: the cumulative window's start (the
/// exporter's birth) and the report's emission, in Unix nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub start_unix_nano: u64,
    pub time_unix_nano: u64,
}

/// The exporter's own health, exported beside the report's families.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Health {
    /// Reports a full hand-off dropped before reaching the push task.
    pub reports_dropped: u64,
    /// Pushes that failed (no connection, a timeout, a non-2xx status).
    pub push_failures: u64,
}

/// Map `report` to one export request for the service `service`.
pub fn request(
    report: &MetricReport,
    service: &str,
    at: Stamp,
    health: Health,
) -> proto::ExportMetricsServiceRequest {
    let mut m = Vec::with_capacity(96);
    let f = &families::METRICS_DROPPED;
    m.push(scalar(f.name, f.kind, f.help, (f.get)(report), at));
    if let Some(reg) = &report.registry {
        for f in &families::REGISTRY {
            m.push(scalar(f.name, f.kind, f.help, (f.get)(reg), at));
        }
    }
    for f in &families::NET {
        m.push(scalar(f.name, f.kind, f.help, (f.get)(&report.net), at));
    }
    let closes = report.net.server_closes.iter();
    let points = closes.map(|(why, n)| int_point(vec![attr("reason", why.label())], n, at, true));
    m.push(sum(SERVER_CLOSES.0, SERVER_CLOSES.1, points.collect()));
    for f in &families::TRANSPORT {
        m.push(scalar(
            f.name,
            f.kind,
            f.help,
            (f.get)(&report.transport),
            at,
        ));
    }

    let rooms = &report.rooms;
    if !rooms.is_empty() {
        for f in families::room() {
            m.push(match f.value {
                RoomValue::Counter(get) => {
                    let pts = rooms
                        .iter()
                        .map(|r| int_point(vec![room(r)], get(r), at, true));
                    sum(f.name, f.help, pts.collect())
                }
                RoomValue::Gauge(get) => {
                    let pts = rooms.iter().map(|r| double_point(room(r), get(r), at));
                    gauge(f.name, f.help, pts.collect())
                }
                RoomValue::StepHist => histogram(f.name, f.help, rooms, at, step_hist),
                RoomValue::StepFine => histogram(f.name, FINE_HELP, rooms, at, step_fine),
            });
        }
        logic(&mut m, rooms, at);
    }

    for (name, help, v) in [
        (
            "gsb_export_otlp_reports_dropped",
            "Reports this exporter dropped on its full hand-off to the push task, cumulative.",
            health.reports_dropped,
        ),
        (
            "gsb_export_otlp_push_failures",
            "OTLP pushes that failed (connect, timeout or non-2xx status), cumulative.",
            health.push_failures,
        ),
    ] {
        m.push(scalar(name, Kind::Counter, help, v, at));
    }

    let resource = proto::Resource {
        attributes: vec![attr("service.name", service)],
        dropped_attributes_count: 0,
    };
    let scope = proto::InstrumentationScope {
        name: SCOPE.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
    };
    proto::ExportMetricsServiceRequest {
        resource_metrics: vec![proto::ResourceMetrics {
            resource: Some(resource),
            scope_metrics: vec![proto::ScopeMetrics {
                scope: Some(scope),
                metrics: m,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

/// The logic's own counters (F9): one metric per name in the order the
/// names first appear, SUM as a monotonic sum, MAX as a gauge; then the
/// bound's overflow gauge, only while some room dropped names.
fn logic(m: &mut Vec<proto::Metric>, rooms: &[RoomReport], at: Stamp) {
    let mut names: Vec<(LogicCounter, Vec<proto::NumberDataPoint>)> = Vec::new();
    for r in rooms {
        for s in r.logic.slots() {
            let max = s.counter.fold() == LogicFold::Max;
            let point = int_point(vec![room(r)], s.value, at, !max);
            match names.iter_mut().find(|n| n.0.name() == s.counter.name()) {
                Some(n) => n.1.push(point),
                None => names.push((s.counter, vec![point])),
            }
        }
    }
    for (c, points) in names {
        let name = format!("gsb_room_logic_{}", c.name());
        let (help, max) = match c.fold() {
            LogicFold::Sum => (LOGIC_SUM_HELP, false),
            LogicFold::Max => (LOGIC_MAX_HELP, true),
        };
        let help = if c.help().is_empty() { help } else { c.help() };
        m.push(if max {
            gauge(&name, help, points)
        } else {
            sum(&name, help, points)
        });
    }
    let dropped = rooms.iter().filter(|r| r.logic.dropped() > 0);
    let points: Vec<_> = dropped
        .map(|r| int_point(vec![room(r)], u64::from(r.logic.dropped()), at, false))
        .collect();
    if !points.is_empty() {
        m.push(gauge(LOGIC_DROPPED.0, LOGIC_DROPPED.1, points));
    }
}

/// The log2 histogram of one room: its budget edges in µs.
fn step_hist(r: &RoomReport) -> (Vec<f64>, Vec<u64>) {
    let bounds = (0..HIST_EDGES.len()).map(|i| hist_edge_us(r.budget_us, i) as f64);
    (bounds.collect(), r.step_hist.to_vec())
}

/// The fine histogram of one room: its fixed bins plus one bucket for
/// the steps at/above the cap (the rest of the log2 population).
fn step_fine(r: &RoomReport) -> (Vec<f64>, Vec<u64>) {
    let bounds = (1..=FINE_HIST_BINS as u64).map(|i| (i * FINE_HIST_US_PER_BIN) as f64);
    let mut counts = r.step_fine_hist.to_vec();
    let fine: u64 = counts.iter().sum();
    counts.push(r.step_hist.iter().sum::<u64>().saturating_sub(fine));
    (bounds.collect(), counts)
}

/// One histogram metric, a point per room from `shape`.
fn histogram(
    name: &str,
    help: &str,
    rooms: &[RoomReport],
    at: Stamp,
    shape: fn(&RoomReport) -> (Vec<f64>, Vec<u64>),
) -> proto::Metric {
    let points = rooms.iter().map(|r| {
        let (explicit_bounds, bucket_counts) = shape(r);
        let count = bucket_counts.iter().sum::<u64>();
        proto::HistogramDataPoint {
            attributes: vec![room(r)],
            start_time_unix_nano: at.start_unix_nano,
            time_unix_nano: at.time_unix_nano,
            count,
            // mean × steps: the exact total, as the exposition's `_sum`.
            sum: Some(r.step_mean_us * r.steps as f64),
            bucket_counts,
            explicit_bounds,
            flags: 0,
            min: (count > 0).then_some(r.step_min_us as f64),
            max: (count > 0).then_some(r.step_max_us as f64),
        }
    });
    let data = Data::Histogram(proto::Histogram {
        data_points: points.collect(),
        aggregation_temporality: proto::AggregationTemporality::Cumulative as i32,
    });
    metric(name, help, data)
}
