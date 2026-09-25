//! The detach-hold ceiling, remote-effect, migration and team-exchange
//! counters reach every consumer of a room report: the log line
//! (`render`) and the Prometheus exposition, each under its own name.

use super::*;

/// The sixteen counters, each with a distinct value, from sample to report
/// to both renderings.
#[test]
fn the_seam_counters_reach_the_line_and_the_exposition() {
    let t = Instant::now();
    let mut s = room_sample(RoomId(1), t, 10);
    let values: [(&str, u64); 16] = [
        ("detach_forced", 2),
        ("effects_applied", 3),
        ("effects_forwarded", 5),
        ("effects_orphaned", 7),
        ("effects_dropped", 11),
        ("effects_refused", 13),
        ("migrations_out", 17),
        ("migrations_in", 19),
        ("migrations_failed", 23),
        ("team_exports", 29),
        ("team_export_drops", 31),
        ("team_export_records", 37),
        ("team_over_cap", 41),
        ("team_imports", 43),
        ("team_import_records", 47),
        ("team_expired", 53),
    ];
    s.detach_forced = 2;
    s.effects_applied = 3;
    s.effects_forwarded = 5;
    s.effects_orphaned = 7;
    s.effects_dropped = 11;
    s.effects_refused = 13;
    s.migrations_out = 17;
    s.migrations_in = 19;
    s.migrations_failed = 23;
    s.team_exports = 29;
    s.team_export_drops = 31;
    s.team_export_records = 37;
    s.team_over_cap = 41;
    s.team_imports = 43;
    s.team_import_records = 47;
    s.team_expired = 53;
    let mut acc = MetricAccumulator::default();
    acc.apply(MetricsEvent::Room(s));
    let report = acc.report(t);

    let line = report
        .render()
        .into_iter()
        .find(|l| l.starts_with("gsb-metric scope=room "))
        .expect("a room line");
    let prom = report.render_prometheus();
    for (key, n) in values {
        assert!(line.contains(&format!(" {key}={n} ")), "{key}: {line}");
        let family = format!("gsb_room_{key}_total");
        assert!(
            prom.contains(&format!("# TYPE {family} counter\n")),
            "{family}: {prom}"
        );
        assert!(
            prom.contains(&format!("{family}{{room=\"r1\"}} {n}\n")),
            "{family}"
        );
    }
}
