//! The Prometheus exposition: family naming rules, values, and what an
//! empty report must omit.

use super::*;

/// The Prometheus renderer: one HELP/TYPE pair per family, known values
/// on the sample lines, per-room metrics labeled (`room="r<id>"`), the
/// log2 histogram exported with real µs bucket edges, and the fine
/// histogram summarized as p50/p99.
#[test]
fn prometheus_render_exposes_families_and_values() {
    let mut acc = MetricAccumulator::default();
    let t = Instant::now();
    acc.apply(MetricsEvent::Registry(RegistrySample {
        rooms: 2,
        conns: 5,
        rooms_created: 2,
        rooms_destroyed: 1,
        rooms_died: 0,
        joins: 7,
        leaves: 2,
        opens: 5,
        closes: 0,
        metrics_dropped: 0,
    }));
    // Deterministic fine-histogram shape: 100 steps, p50 in bin 10
    // (lower edge 80 µs), p99 in bin 40 (lower edge 320 µs). The COARSE
    // log2 histogram must tell the same story with the same total:
    // production increments exactly one coarse bin per step
    // (`hist_index` caps at the overflow bin), so `sum(step_hist)` ==
    // `steps` is a production invariant — an all-zero coarse table
    // under non-zero steps is an unreachable state.
    let mut s = room_sample(RoomId(1), t, 100);
    s.step_fine_hist[10] = 60;
    s.step_fine_hist[40] = 40;
    s.step_hist[hist_index(33_333, 80)] = 60;
    s.step_hist[hist_index(33_333, 320)] = 40;
    acc.apply(MetricsEvent::Room(s));

    let out = acc.report(t).render_prometheus();

    // Family headers exist exactly once per metric.
    assert_eq!(
        out.matches("# TYPE gsb_registry_rooms gauge").count(),
        1,
        "one TYPE header per family: {out}"
    );
    assert!(out.contains("gsb_registry_rooms 2\n"));
    assert!(out.contains("gsb_registry_conns 5\n"));
    assert!(out.contains("gsb_registry_opens_total 5\n"));
    assert!(out.contains("# TYPE gsb_room_steps_total counter\n"));
    assert!(out.contains("gsb_room_steps_total{room=\"r1\"} 100\n"));
    assert!(out.contains("gsb_room_hz{room=\"r1\"} 0\n"), "no previous sample ⇒ hz gauge 0");
    assert!(out.contains("gsb_net_bytes_in_total 0\n"));

    // Histogram: budget 33 333 µs ⇒ first edge ceil(33333/128) = 261.
    // The crafted steps sit in the coarse bins matching the fine story
    // (80 µs / 320 µs), so every bin below the first edge is empty and
    // +Inf carries the full count — production's "one bin per step"
    // invariant keeps bucket cumulatives and _count consistent.
    assert_eq!(
        hist_edge_us(33_333, 0),
        261,
        "sanity: the first budget-fraction edge at this budget"
    );
    // The two crafted coarse bins render their cumulative counts at
    // their own budget-fraction edges (60 through the 80 µs bin, 100
    // through the 320 µs bin — bins are cumulative in le order):
    let b80 = hist_index(33_333, 80);
    let b320 = hist_index(33_333, 320);
    assert!(
        out.contains(&format!(
            "gsb_room_step_hist_bucket{{room=\"r1\",le=\"{}\"}} 60\n",
            hist_edge_us(33_333, b80)
        )),
        "the 60-step coarse bin renders cumulatively at its own edge"
    );
    assert!(
        out.contains(&format!(
            "gsb_room_step_hist_bucket{{room=\"r1\",le=\"{}\"}} 100\n",
            hist_edge_us(33_333, b320)
        )),
        "the second bin's cumulative includes the first group"
    );
    assert!(out.contains("gsb_room_step_hist_bucket{room=\"r1\",le=\"+Inf\"} 100\n"));
    assert!(out.contains("gsb_room_step_hist_count{room=\"r1\"} 100\n"));
    assert!(out.starts_with("# HELP gsb_metrics_dropped_total"));

    // Summary: exact percentiles of the crafted distribution (bin lower
    // edges, integer arithmetic).
    assert!(out.contains("gsb_room_step_duration_us{room=\"r1\",quantile=\"0.5\"} 80\n"));
    assert!(out.contains("gsb_room_step_duration_us{room=\"r1\",quantile=\"0.99\"} 320\n"));
}

/// Naming convention lock: EVERY counter-family sample line carries the
/// `*_total` suffix (OPS §3); gauges never need it.
#[test]
fn prometheus_counter_families_all_end_in_total() {
    let mut acc = MetricAccumulator::default();
    let t = Instant::now();
    acc.apply(MetricsEvent::Registry(RegistrySample {
        rooms: 1,
        conns: 0,
        rooms_created: 1,
        rooms_destroyed: 0,
        rooms_died: 0,
        joins: 0,
        leaves: 0,
        opens: 0,
        closes: 0,
        metrics_dropped: 0,
    }));
    acc.apply(MetricsEvent::Room(room_sample(RoomId(4), t, 12)));
    let out = acc.report(t).render_prometheus();

    // Parse the exposition minimally: family kind per name, then every
    // sample line must obey its family's convention.
    let mut kinds = std::collections::HashMap::new();
    let mut counters = 0usize;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let mut it = rest.split_whitespace();
            if let (Some(name), Some(kind)) = (it.next(), it.next()) {
                kinds.insert(name.to_owned(), kind.to_owned());
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            let name = line.split(['{', ' ']).next().expect("sample has a name");
            // Histogram/summary exposition derives suffixed sample
            // names (_bucket/_sum/_count) from one declared family.
            let base = name
                .strip_suffix("_bucket")
                .or_else(|| name.strip_suffix("_sum"))
                .or_else(|| name.strip_suffix("_count"))
                .unwrap_or(name);
            let kind = kinds
                .get(base)
                .unwrap_or_else(|| panic!("sample `{name}` lacks a TYPE header"));
            if kind == "counter" {
                counters += 1;
                assert!(
                    name.ends_with("_total"),
                    "counter sample `{name}` violates the *_total suffix"
                );
                assert_eq!(
                    base, name,
                    "a counter never uses histogram-style suffixes: {name}"
                );
            }
        }
    }
    assert!(counters > 20, "the suite covers the counter families: {counters}");
}

/// An empty report (the watch placeholder / a server with no registry
/// sample yet) renders the top-level and net families only — no absent
/// scope leaks an empty family, no room label appears.
#[test]
fn prometheus_empty_report_omits_absent_scopes() {
    let report = MetricReport::initial_stale(Duration::from_secs(1));
    let out = report.render_prometheus();
    assert!(!out.contains("gsb_registry_"), "no registry scope: {out}");
    assert!(!out.contains("{room="), "no room families: {out}");
    assert!(out.contains("# TYPE gsb_net_bytes_in_total counter\n"));
    assert!(out.contains("gsb_metrics_dropped_total 0\n"));
}

/// The watch placeholder is born stale: any freshness threshold of N
/// periods must already see it as expired, so `/healthz` answers
/// honestly ("warming up") until the collector's first REAL emission.
#[test]
fn initial_stale_placeholder_is_past_any_threshold() {
    let period = Duration::from_millis(200);
    let report = MetricReport::initial_stale(period);
    assert!(report.rooms.is_empty());
    assert!(
        report.emitted_at.elapsed() >= period * 3,
        "placeholder age {:?} must exceed three periods",
        report.emitted_at.elapsed()
    );
}
