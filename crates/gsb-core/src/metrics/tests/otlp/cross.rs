//! The two exporters say the same thing: for one report, the OTLP
//! request holds exactly the exposition's families, in its order, each
//! with the matching kind, description and values — the histogram's
//! cumulative buckets, edges, count and sum, and the summary's p50/p99
//! re-derived from the OTLP fine buckets. (Only the exporter's own
//! health metrics, `gsb_export_otlp_*`, exist on one side.)

use super::*;

/// One exposition family: its name, type, help and sample lines as
/// (series name, labels `k=v,k=v`, value).
struct Family {
    name: String,
    kind: String,
    help: String,
    samples: Vec<(String, String, f64)>,
}

fn parse_exposition(text: &str) -> Vec<Family> {
    let mut out: Vec<Family> = Vec::new();
    let mut help = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# HELP ") {
            help = rest.split_once(' ').expect("help text").1.to_owned();
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').expect("type word");
            out.push(Family {
                name: name.to_owned(),
                kind: kind.to_owned(),
                help: std::mem::take(&mut help),
                samples: Vec::new(),
            });
        } else {
            let (series, value) = line.rsplit_once(' ').expect("sample value");
            let (name, labels) = match series.split_once('{') {
                Some((n, l)) => (n, l.trim_end_matches('}').replace('"', "")),
                None => (series, String::new()),
            };
            let value = match value {
                "+Inf" => f64::INFINITY,
                v => v.parse().expect("numeric sample"),
            };
            let fam = out.last_mut().expect("a sample after its TYPE");
            fam.samples.push((name.to_owned(), labels, value));
        }
    }
    out
}

/// The label set without one key (`le`, `quantile`), and that key's value.
fn split_label(labels: &str, key: &str) -> (String, String) {
    let mut rest = Vec::new();
    let mut hit = String::new();
    for kv in labels.split(',') {
        match kv.strip_prefix(&format!("{key}=")) {
            Some(v) => hit = v.to_owned(),
            None => rest.push(kv),
        }
    }
    (rest.join(","), hit)
}

fn number_value(p: &proto::NumberDataPoint) -> f64 {
    match p.value {
        Some(Value::AsInt(v)) => v as f64,
        Some(Value::AsDouble(v)) => v,
        None => panic!("a point without a value"),
    }
}

fn check(report: &MetricReport) {
    let fams = parse_exposition(&report.render_prometheus());
    let req = map::request(report, "gsb", AT, Health::default());
    let otlp: Vec<&proto::Metric> = metrics(&req)
        .iter()
        .filter(|m| !m.name.starts_with("gsb_export_otlp_"))
        .collect();
    let names: Vec<&str> = fams.iter().map(|f| f.name.as_str()).collect();
    let otlp_names: Vec<String> = otlp.iter().map(|m| m.name.clone()).collect();
    let expected: Vec<&str> = names
        .iter()
        .map(|n| n.strip_suffix("_total").unwrap_or(n))
        .collect();
    assert_eq!(otlp_names, expected, "same families, same order");

    for (f, m) in fams.iter().zip(&otlp) {
        let data = m.data.as_ref().expect("data");
        match (f.kind.as_str(), data) {
            ("counter", Data::Sum(s)) => {
                assert!(s.is_monotonic, "{}", f.name);
                assert_eq!(s.aggregation_temporality, 2, "{} cumulative", f.name);
                points_match(f, &s.data_points);
            }
            ("gauge", Data::Gauge(g)) => points_match(f, &g.data_points),
            ("histogram", Data::Histogram(h)) => histogram_match(f, h),
            ("summary", Data::Histogram(h)) => summary_match(f, h),
            (kind, _) => panic!("{}: {kind} mapped to the wrong OTLP kind", f.name),
        }
        if f.kind != "summary" {
            assert_eq!(m.description, f.help, "{}", f.name);
        }
    }
}

/// Every sample line has the OTLP point with its labels and value.
fn points_match(f: &Family, points: &[proto::NumberDataPoint]) {
    assert_eq!(points.len(), f.samples.len(), "{}", f.name);
    for (p, (_, labels, v)) in points.iter().zip(&f.samples) {
        assert_eq!(&attrs(&p.attributes).replace(' ', ""), labels, "{}", f.name);
        assert_eq!(number_value(p), *v, "{} {labels}", f.name);
    }
}

/// Per room: the cumulative `_bucket` series are the running sums of the
/// OTLP buckets, the finite `le`s are its bounds, and count/sum agree.
fn histogram_match(f: &Family, h: &proto::Histogram) {
    assert_eq!(h.aggregation_temporality, 2);
    for p in &h.data_points {
        let room = attrs(&p.attributes);
        let buckets = series(f, "_bucket", &room);
        let mut cum = 0u64;
        let running: Vec<f64> = p
            .bucket_counts
            .iter()
            .map(|c| {
                cum += c;
                cum as f64
            })
            .collect();
        assert_eq!(
            buckets.iter().map(|b| b.1).collect::<Vec<_>>(),
            running,
            "{room}"
        );
        let les: Vec<f64> = buckets.iter().map(|b| b.0.parse().unwrap()).collect();
        assert_eq!(&les[..les.len() - 1], &p.explicit_bounds[..], "{room}");
        assert_eq!(series(f, "_count", &room)[0].1, p.count as f64);
        assert_eq!(Some(series(f, "_sum", &room)[0].1), p.sum);
    }
}

/// One room's `<name><suffix>` series as (`le`, value).
fn series(f: &Family, suffix: &str, room: &str) -> Vec<(String, f64)> {
    let name = format!("{}{suffix}", f.name);
    f.samples
        .iter()
        .filter(|(n, l, _)| *n == name && split_label(l, "le").0 == room)
        .map(|(_, l, v)| (split_label(l, "le").1, *v))
        .collect()
}

/// The summary's p50/p99 come out of the OTLP fine buckets unchanged.
fn summary_match(f: &Family, h: &proto::Histogram) {
    for p in &h.data_points {
        let room = attrs(&p.attributes);
        let fine = &p.bucket_counts[..FINE_HIST_BINS];
        for (_, labels, v) in f
            .samples
            .iter()
            .filter(|s| split_label(&s.1, "quantile").0 == room)
        {
            let p_of = match split_label(labels, "quantile").1.as_str() {
                "0.5" => 50,
                "0.99" => 99,
                q => panic!("unexpected quantile {q}"),
            };
            let us = fine_hist_percentile_us(fine, p.count, p_of).expect("in range");
            assert_eq!(us as f64, *v, "{room} p{p_of}");
        }
    }
}

#[test]
fn the_golden_report_says_the_same_on_both_exporters() {
    check(&super::golden::report());
}

/// A logic over the per-room bound: the overflow gauge (only present
/// while non-zero) and the names that fit, on both sides.
#[test]
fn a_logic_overflow_says_the_same_on_both_exporters() {
    let mut acc = MetricAccumulator::default();
    let t = Instant::now();
    let mut s = room_sample(RoomId(1), t, 10);
    for i in 0..LOGIC_COUNTERS_MAX + 2 {
        let fold = if i % 3 == 0 {
            LogicFold::Max
        } else {
            LogicFold::Sum
        };
        let c = LogicCounter::parse(&format!("c{i}"), "", fold).unwrap();
        s.logic.put(&c, i as u64);
    }
    acc.apply(MetricsEvent::Room(s));
    let report = acc.report(t);
    assert_eq!(report.rooms[0].logic.dropped(), 2);
    check(&report);
}
