//! The metrics text of a server whose logics declare no counters of
//! their own, pinned byte for byte: the `gsb-metric` lines and the
//! Prometheus exposition. The expected text was produced by the code
//! BEFORE the logic-counter seam existed (F9), so this test is the
//! proof that the seam adds nothing to the output of a logic that does
//! not use it. A core counter added since is in it on purpose, with the
//! one key and family it added: F15's `req_refused=` /
//! `gsb_room_requests_refused_congested_total`. So are the two families
//! B39 added for counters the line already carried (`shipped_frames=`,
//! `private_frames=`): `gsb_room_shipped_frames_total`,
//! `gsb_room_private_frames_total`. And B36's `req_unread=` /
//! `gsb_room_requests_dropped_unread_total`.

use super::*;
use crate::conn::ServerClose;

/// A report with every scope present and distinct values: a registry,
/// two rooms (the first sampled twice, so its rates are real numbers),
/// connection traffic, one server close and one flooder.
pub(super) fn golden_report() -> MetricReport {
    let mut acc = MetricAccumulator::default();
    let t0 = Instant::now();
    acc.apply(MetricsEvent::Registry(RegistrySample {
        rooms: 2,
        conns: 3,
        rooms_created: 2,
        rooms_destroyed: 0,
        rooms_died: 0,
        joins: 3,
        leaves: 1,
        opens: 4,
        closes: 1,
        metrics_dropped: 0,
    }));
    let mut a = room_sample(RoomId(1), t0, 30);
    a.step_min_us = 40;
    a.step_max_us = 900;
    a.step_sum_us = 3_000;
    a.step_hist[hist_index(33_333, 100)] = 30;
    a.step_fine_hist[12] = 30;
    a.late_min_us = 5;
    a.late_max_us = 70;
    a.late_sum_us = 600;
    a.snapshots = 29;
    a.snap_bytes = 2_900;
    a.snap_bytes_max = 120;
    a.snap_records = 58;
    a.shipped_bytes = 8_700;
    a.shipped_frames = 87;
    a.private_frames = 3;
    a.joins = 3;
    a.leaves = 1;
    a.groups = 1;
    a.members = 2;
    a.max_group = 2;
    a.requests_local = 4;
    acc.apply(MetricsEvent::Room(a));
    acc.report(t0);
    let t1 = t0 + Duration::from_secs(1);
    let mut a2 = a;
    a2.emit_at = t1;
    a2.steps = 60;
    a2.step_hist[hist_index(33_333, 100)] = 60;
    a2.step_fine_hist[12] = 60;
    a2.step_sum_us = 6_000;
    a2.late_sum_us = 1_200;
    a2.snap_bytes = 5_900;
    a2.shipped_bytes = 17_700;
    a2.dropped_frames = 2;
    a2.requests_refused_congested = 3;
    a2.requests_dropped_unread = 2;
    acc.apply(MetricsEvent::Room(a2));
    let mut b = room_sample(RoomId(7), t1, 15);
    b.effects_applied = 6;
    b.migrations_out = 2;
    b.team_exports = 9;
    b.team_over_budget = 1;
    b.detached = 1;
    b.resumes = 2;
    b.pending_requests = 1;
    acc.apply(MetricsEvent::Room(b));
    acc.apply(MetricsEvent::Conn(ConnSample {
        conn: ConnectionId(1),
        bytes_in: 400,
        bytes_out: 60,
        frames_in: 20,
        frames_out: 3,
        actions_dropped: 5,
        metrics_dropped: 0,
        violations: 1,
        input_rate_limited: 9,
        server_close: None,
        last: false,
    }));
    acc.apply(MetricsEvent::Conn(ConnSample {
        conn: ConnectionId(2),
        bytes_in: 10,
        bytes_out: 0,
        frames_in: 1,
        frames_out: 0,
        actions_dropped: 0,
        metrics_dropped: 0,
        violations: 0,
        input_rate_limited: 0,
        server_close: Some(ServerClose::IdleTimeout),
        last: true,
    }));
    acc.report(t1)
}

/// Both renderings, joined as one text: the lines, a blank line, then
/// the exposition.
pub(super) fn golden_text(report: &MetricReport) -> String {
    let mut text = report.render().join("\n");
    text.push_str("\n\n");
    text.push_str(&report.render_prometheus());
    text
}

/// The pinned text, byte for byte.
#[test]
fn a_report_without_logic_counters_renders_exactly_the_pinned_text() {
    let text = golden_text(&golden_report());
    let pinned = include_str!("golden/no_logic_counters.txt");
    if text != pinned {
        for (i, (a, b)) in text.lines().zip(pinned.lines()).enumerate() {
            assert_eq!(a, b, "first differing line: {}", i + 1);
        }
    }
    assert_eq!(text, pinned);
}
