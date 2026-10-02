//! The server-close family through the collector: per-reason counts
//! from the connection actors' FINAL samples, summed into
//! `NetReport::server_closes`, and exported as one labeled Prometheus
//! family plus stable per-reason keys on the log line.

use super::*;
use crate::conn::ServerClose;

fn final_sample(conn: u64, server_close: Option<ServerClose>) -> ConnSample {
    ConnSample {
        conn: ConnectionId(conn),
        bytes_in: 0,
        bytes_out: 0,
        frames_in: 0,
        frames_out: 0,
        actions_dropped: 0,
        metrics_dropped: 0,
        violations: 0,
        input_rate_limited: 0,
        actions_dropped_closed: 0,
        requests_dropped_closed: 0,
        requests_dropped_full: 0,
        requests_no_room: 0,
        heartbeats_throttled_preauth: 0,
        heartbeats_throttled_authed: 0,
        frames_out_closed: 0,
        close_notices_dropped: 0,
        requests_unprocessed: 0,
        actions_unprocessed: 0,
        control_frames_unprocessed: 0,
        server_close,
        last: true,
        tickets: Default::default(),
    }
}

/// Three write stalls, one idle close and one CLIENT-side close: the
/// report counts exactly the four verdicts, each under its own reason,
/// and the client close nowhere.
#[test]
fn server_closes_are_counted_by_reason_and_client_closes_are_not() {
    let mut acc = MetricAccumulator::default();
    for conn in 1..=3 {
        acc.apply(MetricsEvent::Conn(final_sample(
            conn,
            Some(ServerClose::WriteStall),
        )));
    }
    acc.apply(MetricsEvent::Conn(final_sample(
        4,
        Some(ServerClose::IdleTimeout),
    )));
    acc.apply(MetricsEvent::Conn(final_sample(5, None)));
    let report = acc.report(Instant::now());
    let c = report.net.server_closes;

    assert_eq!(c.get(ServerClose::WriteStall), 3);
    assert_eq!(c.get(ServerClose::IdleTimeout), 1);
    assert_eq!(c.total(), 4, "the client-side close is not counted: {c:?}");
    assert_eq!(c.nonzero_summary(), "idle_timeout:1,write_stall:3");

    // Prometheus: ONE family, a `reason` label per sample, zeros included.
    let prom = report.render_prometheus();
    assert_eq!(
        prom.matches("# TYPE gsb_net_server_closes_total counter\n")
            .count(),
        1,
        "one family, not one metric per reason: {prom}"
    );
    assert!(prom.contains("gsb_net_server_closes_total{reason=\"write_stall\"} 3\n"));
    assert!(prom.contains("gsb_net_server_closes_total{reason=\"idle_timeout\"} 1\n"));
    assert!(
        prom.contains("gsb_net_server_closes_total{reason=\"violation_budget\"} 0\n"),
        "a known reason is exported at zero too: {prom}"
    );
    assert_eq!(
        prom.matches("gsb_net_server_closes_total{reason=").count(),
        ServerClose::COUNT
    );

    // The log line: the total plus one stable key per reason.
    let lines = report.render();
    let net = lines
        .iter()
        .find(|l| l.contains("scope=net") && l.contains("server_closes="))
        .expect("the net line carries the server-close total");
    assert!(net.contains(" server_closes=4 "), "{net}");
    assert!(net.contains(" server_close_write_stall=3"), "{net}");
    assert!(net.contains(" server_close_idle_timeout=1"), "{net}");
    assert!(net.contains(" server_close_outbound_dead=0"), "{net}");
}
