//! The OTLP mapping of the golden report (`tests::golden`) plus two
//! logic counters, pinned as a readable dump: every metric's name,
//! kind, temporality, monotonicity and description, and every point's
//! attributes, stamps and values. A change to how any family maps —
//! kind, name, value, bounds — shows up here as a changed line.

use super::*;
use crate::metrics::tests::golden::golden_report;

const KILLS: LogicCounter = LogicCounter::sum("kills", "Players felled, cumulative.");
const PEAK: LogicCounter = LogicCounter::max("fights_peak", "");

/// The golden report with a SUM on room 1 and a MAX on room 7.
pub(super) fn report() -> MetricReport {
    let mut r = golden_report();
    r.rooms[0].logic.put(&KILLS, 3);
    r.rooms[1].logic.put(&PEAK, 5);
    r
}

/// Bounds as text: a list, or `n×step` for an even ladder from `step`.
fn bounds(b: &[f64]) -> String {
    let step = b.first().copied().unwrap_or(0.0);
    let even = b.len() > 16
        && b.iter()
            .enumerate()
            .all(|(i, &x)| x == step * (i + 1) as f64);
    if even {
        format!("{}x{step}", b.len())
    } else {
        format!("{b:?}")
    }
}

/// Counts as text: the non-zero buckets `index:count` of `n` buckets.
fn counts(c: &[u64]) -> String {
    let nz: Vec<String> = c
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(i, n)| format!("{i}:{n}"))
        .collect();
    format!("{}[{}]", c.len(), nz.join(" "))
}

/// The readable dump of a request.
pub(super) fn dump(req: &proto::ExportMetricsServiceRequest) -> String {
    let rm = &req.resource_metrics[0];
    let res = rm.resource.as_ref().expect("resource");
    let scope = rm.scope_metrics[0].scope.as_ref().expect("scope");
    let mut out = format!(
        "resource {}\nscope {}\n",
        attrs(&res.attributes),
        scope.name
    );
    for m in metrics(req) {
        let (kind, n) = match m.data.as_ref().expect("data") {
            Data::Gauge(g) => ("gauge".to_owned(), g.data_points.len()),
            Data::Sum(s) => (
                format!(
                    "sum t={} mono={}",
                    s.aggregation_temporality, s.is_monotonic
                ),
                s.data_points.len(),
            ),
            Data::Histogram(h) => (
                format!("histogram t={}", h.aggregation_temporality),
                h.data_points.len(),
            ),
        };
        out.push_str(&format!(
            "{} {kind} unit={:?} n={n} | {}\n",
            m.name, m.unit, m.description
        ));
        match m.data.as_ref().expect("data") {
            Data::Gauge(proto::Gauge { data_points: ps })
            | Data::Sum(proto::Sum {
                data_points: ps, ..
            }) => {
                for p in ps {
                    out.push_str(&format!(
                        "  [{}] {}..{} {}\n",
                        attrs(&p.attributes),
                        p.start_time_unix_nano,
                        p.time_unix_nano,
                        number(p)
                    ));
                }
            }
            Data::Histogram(h) => {
                for p in &h.data_points {
                    out.push_str(&format!(
                        "  [{}] {}..{} count={} sum={:?} min={:?} max={:?} bounds={} counts={}\n",
                        attrs(&p.attributes),
                        p.start_time_unix_nano,
                        p.time_unix_nano,
                        p.count,
                        p.sum,
                        p.min,
                        p.max,
                        bounds(&p.explicit_bounds),
                        counts(&p.bucket_counts)
                    ));
                }
            }
        }
    }
    out
}

/// The pinned mapping, line for line.
#[test]
fn the_golden_report_maps_to_the_pinned_otlp() {
    let health = Health {
        reports_dropped: 4,
        push_failures: 2,
    };
    let text = dump(&map::request(&report(), "gsb-test", AT, health));
    let pinned = include_str!("golden/otlp.txt");
    for (i, (a, b)) in text.lines().zip(pinned.lines()).enumerate() {
        assert_eq!(a, b, "first differing line: {}", i + 1);
    }
    assert_eq!(text.lines().count(), pinned.lines().count(), "{text}");
}
