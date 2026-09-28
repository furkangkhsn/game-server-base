//! The Prometheus text exposition the ops surface serves at /metrics:
//! one HELP/TYPE pair per family, room ids as labels.
//!
//! The pull exporter of the export seam: the families — names, types,
//! helps, order — are the shared vocabulary (`families`); this file is
//! only the text format.

use std::fmt::Write as _;

use super::families::{self, RoomFamily, RoomValue, Scalar};
use crate::metrics::*;

mod closes;
mod logic;

impl MetricReport {
    /// Render as Prometheus text exposition format, version 0.0.4 (the
    /// scrape format of `/metrics`; see `docs/OPS.md` §3/§5): a
    /// `# HELP` / `# TYPE` header pair per metric family followed by its
    /// sample lines.
    ///
    /// Naming follows OPS §3's `<scope>_<counter>` vocabulary with the
    /// `gsb_` prefix and `*_total` on every counter; per-room samples
    /// carry the room as a LABEL (`gsb_room_steps_total{room="r1"}`) rather
    /// than embedding the id in the metric NAME (`gsb_room_r1_steps_total`):
    /// label is the canonical exposition practice — an id inside the name
    /// would mint one unbounded metric-name series per room per scrape,
    /// while the label keeps one family per counter and lets consumers
    /// aggregate or filter identically.
    ///
    /// Distributions: the budget-relative log2 step histogram exports as a
    /// classic histogram (`*_bucket` with cumulative counts, `le` in real µs
    /// derived from each room's `budget_us`, plus `_sum`/`_count`); the fine
    /// fixed-bin histogram — whose whole design purpose is sub-budget
    /// percentile resolution (see [`FINE_HIST_BINS`]) — exports as a summary
    /// (p50/p99 via [`fine_hist_percentile_us`]). A quantile whose rank
    /// falls beyond the fine histogram's cap has no fine value; the line is
    /// omitted rather than guessed (the log2 histogram still carries the
    /// overflow signal).
    pub fn render_prometheus(&self) -> String {
        let mut out = String::with_capacity(4096);

        // Top-level: metric-channel health; then the registry scope
        // (control-plane gauges + cumulative counters) and the net scope.
        scalar(&mut out, &families::METRICS_DROPPED, self);
        if let Some(reg) = &self.registry {
            for f in &families::REGISTRY {
                scalar(&mut out, f, reg);
            }
            let lost = &reg.verdicts_lost.closes;
            closes::render(&mut out, families::CLOSE_VERDICTS_LOST, lost);
        }
        for f in &families::NET {
            scalar(&mut out, f, &self.net);
        }
        closes::render(&mut out, families::SERVER_CLOSES, &self.net.server_closes);
        for f in &families::TRANSPORT {
            scalar(&mut out, f, &self.transport);
        }

        let rooms = &self.rooms;
        if rooms.is_empty() {
            return out;
        }
        for f in families::room() {
            match f.value {
                RoomValue::Counter(get) => family(&mut out, f, "counter", rooms, |r| get(r) as f64),
                RoomValue::Gauge(get) => family(&mut out, f, "gauge", rooms, get),
                RoomValue::StepHist => step_hist(&mut out, f, rooms),
                RoomValue::StepFine => step_summary(&mut out, f, rooms),
            }
        }
        logic::render(&mut out, rooms);

        out
    }
}

/// Format a float as a Prometheus sample value (finite values print
/// plainly; non-finite ones use the format's literals).
fn val(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 {
            "+Inf".to_owned()
        } else {
            "-Inf".to_owned()
        }
    } else {
        format!("{v}")
    }
}

/// The `# HELP` / `# TYPE` pair.
fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = write!(out, "# HELP {name} {help}\n# TYPE {name} {kind}\n");
}

/// One unlabeled server-wide family.
fn scalar<T>(out: &mut String, f: &Scalar<T>, from: &T) {
    header(out, f.name, f.kind.word(), f.help);
    let _ = writeln!(out, "{} {}", f.name, (f.get)(from));
}

/// One labeled-per-room family: header once, then a sample line per room
/// (`room="r<id>"` label).
fn family(
    out: &mut String,
    f: &RoomFamily,
    kind: &str,
    rooms: &[RoomReport],
    get: impl Fn(&RoomReport) -> f64,
) {
    header(out, f.name, kind, f.help);
    for r in rooms {
        let _ = writeln!(out, "{}{{room=\"r{}\"}} {}", f.name, r.room.0, val(get(r)));
    }
}

/// The budget-relative log2 histogram, exported with REAL microsecond
/// bucket edges (derived per room from `budget_us` so the scrape reads in
/// absolute units) and cumulative counts, as the histogram format
/// requires.
fn step_hist(out: &mut String, f: &RoomFamily, rooms: &[RoomReport]) {
    let name = f.name;
    header(out, name, "histogram", f.help);
    for r in rooms {
        let mut cum = 0u64;
        for (i, &cnt) in r.step_hist.iter().enumerate() {
            cum += cnt;
            let le = if i < HIST_EDGES.len() {
                hist_edge_us(r.budget_us, i).to_string()
            } else {
                "+Inf".to_owned()
            };
            let _ = writeln!(
                out,
                "{name}_bucket{{room=\"r{}\",le=\"{}\"}} {cum}",
                r.room.0, le
            );
        }
        // `_sum` in µs: RoomReport carries mean (sum/steps by
        // construction) and steps, so mean × steps reconstructs the
        // exact total duration without widening the report.
        let _ = writeln!(
            out,
            "{name}_sum{{room=\"r{}\"}} {}",
            r.room.0,
            val(r.step_mean_us * r.steps as f64)
        );
        let _ = writeln!(
            out,
            "{name}_count{{room=\"r{}\"}} {}",
            r.room.0,
            r.step_hist.iter().sum::<u64>()
        );
    }
}

/// The fine fixed-bin histogram as a p50/p99 summary (its design
/// purpose — sub-budget resolution).
fn step_summary(out: &mut String, f: &RoomFamily, rooms: &[RoomReport]) {
    let name = f.name;
    header(out, name, "summary", f.help);
    for r in rooms {
        for (q, p) in [("0.5", 50u32), ("0.99", 99)] {
            if let Some(us) = fine_hist_percentile_us(&r.step_fine_hist, r.steps, p) {
                let _ = writeln!(
                    out,
                    "{name}{{room=\"r{}\",quantile=\"{q}\"}} {us}",
                    r.room.0
                );
            }
        }
    }
}
