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
//! `gsb_room_requests_dropped_unread_total`, B32's `sends_closed=` /
//! `gsb_room_sends_closed_total`, and B51's net-scope
//! `actions_dropped_closed=` / `requests_dropped_closed=` with
//! `gsb_net_actions_dropped_closed_total` /
//! `gsb_net_requests_dropped_closed_total`, and B53's `req_undelivered=` /
//! `req_abandoned=` with `gsb_room_requests_undelivered_total` /
//! `gsb_room_requests_abandoned_total`, and B54's `actions_unread=` /
//! `actions_unbound=` / `req_unbound=` with
//! `gsb_room_actions_dropped_{unread,unbound}_total` /
//! `gsb_room_requests_dropped_unbound_total`, and B55's net-scope
//! `requests_dropped_full=` / `requests_no_room=` with
//! `gsb_net_requests_dropped_full_total` /
//! `gsb_net_requests_no_room_total` — plus the one deliberate HELP change
//! of the round: `gsb_net_actions_dropped_total` now says it counts
//! game-band actions only. B56's `hb_throttled_preauth=` /
//! `hb_throttled_authed=` with
//! `gsb_net_heartbeats_throttled_{preauth,authed}_total`. B57's
//! `frames_out_closed=` / `close_notices_dropped=` with
//! `gsb_net_frames_out_closed_total` /
//! `gsb_net_close_notices_dropped_total`, and the registry scope's
//! `join_ops_dropped=` / `close_ops_dropped=` /
//! `match_results_dropped_{full,closed}=` with their four
//! `gsb_registry_*_total` families. B60's `requests_unprocessed=` /
//! `actions_unprocessed=` / `control_frames_unprocessed=` with
//! `gsb_net_{requests,actions,control_frames}_unprocessed_total`. And
//! B62's two HELP changes: `gsb_room_requests_undelivered_total` and
//! `gsb_room_requests_abandoned_total` name the room's stop among the
//! session ends. B58's transport scope: the `gsb-metric scope=transport`
//! line and its eighteen `gsb_transport_*_total` families (the transport
//! tasks' own dropped samples also fold into `gsb_metrics_dropped_total`).
//! B66's seven stream-pump counters at the end of that line and table
//! (`stream_frames_unwritten` .. `ws_frames_dropped_after_close`), and
//! its ten rUDP/verdict counters after them
//! (`udp_game_datagrams_send_failed` .. `writer_verdicts_deferred`).
//! B67's registry-scope `rooms_ended_uncounted=` /
//! `gsb_registry_rooms_ended_uncounted_total`. B68's nine room-scope stop
//! counters (`joins_unprocessed=` .. `border_updates_unapplied=` after
//! `metrics_dropped=`, `gsb_room_*_total` after the session families).
//! B72's registry-scope `team_relays_dropped_{full,closed}=` with
//! `gsb_registry_team_relays_dropped_{full,closed}_total`. B73's HELP
//! change: `gsb_transport_udp_frames_drained_total` says it counts the
//! session's frames, not the demux's piggybacked ACKs. B74's four
//! transport counters at the end of that line and table
//! (`handshakes_cut_closed` .. `udp_sessions_unaccepted_closed`).
//! B75's registry-scope `joins_refused_closed=` with
//! `gsb_registry_joins_refused_closed_total`. B80's two transport
//! counters at the end of that line and table
//! (`ws_going_away_unsent_{closed,stalled}`).

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
        join_ops_dropped: 1,
        close_ops_dropped: 0,
        team_relays_dropped_full: 4,
        team_relays_dropped_closed: 5,
    }));
    // Two match results a full sink refused, one a closed sink did (B57).
    for cause in [
        MatchResultDrop::Full,
        MatchResultDrop::Full,
        MatchResultDrop::Closed,
    ] {
        acc.apply(MetricsEvent::MatchResultDropped(cause));
    }
    // A room task that ended without its final count (B67); no row of
    // its own in this report.
    acc.apply(MetricsEvent::RoomEndedUncounted(RoomId(99)));
    // Two joins a stopped room refused (B75).
    acc.apply(MetricsEvent::JoinRefusedClosed);
    acc.apply(MetricsEvent::JoinRefusedClosed);
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
    a2.sends_closed = 1;
    a2.requests_refused_congested = 3;
    a2.requests_dropped_unread = 2;
    a2.requests_undelivered = 1;
    a2.actions_dropped_unread = 3;
    acc.apply(MetricsEvent::Room(a2));
    let mut b = room_sample(RoomId(7), t1, 15);
    b.effects_applied = 6;
    b.migrations_out = 2;
    b.team_exports = 9;
    b.team_over_budget = 1;
    b.detached = 1;
    b.resumes = 2;
    b.pending_requests = 1;
    b.requests_abandoned = 1;
    b.requests_dropped_unbound = 1;
    b.actions_dropped_unbound = 1;
    // What a stop left (B68; a final sample's, here on a plain one).
    b.stop.joins_unprocessed = 1;
    b.stop.effects_unsent = 2;
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
        actions_dropped_closed: 0,
        requests_dropped_closed: 1,
        requests_dropped_full: 2,
        requests_no_room: 0,
        heartbeats_throttled_preauth: 4,
        heartbeats_throttled_authed: 0,
        frames_out_closed: 1,
        close_notices_dropped: 0,
        requests_unprocessed: 1,
        actions_unprocessed: 2,
        control_frames_unprocessed: 0,
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
        actions_dropped_closed: 2,
        requests_dropped_closed: 0,
        requests_dropped_full: 0,
        requests_no_room: 1,
        heartbeats_throttled_preauth: 0,
        heartbeats_throttled_authed: 6,
        frames_out_closed: 0,
        close_notices_dropped: 1,
        requests_unprocessed: 0,
        actions_unprocessed: 0,
        control_frames_unprocessed: 3,
        server_close: Some(ServerClose::IdleTimeout),
        last: true,
    }));
    // Two transport deltas (B58): every counter distinct, the first one
    // summed across both.
    let mut values = [0u64; TRANSPORT_COUNT];
    for (i, v) in values.iter_mut().enumerate() {
        *v = i as u64 + 1;
    }
    acc.apply(TransportCounters::from_values(values).event());
    acc.apply(
        TransportCounters {
            udp_requests_dropped_full: 10,
            ..Default::default()
        }
        .event(),
    );
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
