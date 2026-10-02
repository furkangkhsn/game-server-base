//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

use super::*;
use crate::id::{ConnectionId, RoomId};
use crate::ticker::TickInfo;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

#[cfg(feature = "prometheus")]
mod closes;
mod collector;
mod cut;
mod export;
mod final_report;
#[cfg(feature = "prometheus")]
mod golden;
mod histogram;
mod logic;
#[cfg(feature = "otlp")]
mod otlp;
#[cfg(feature = "prometheus")]
mod prometheus;
mod pruning;
mod room_final;
#[cfg(feature = "prometheus")]
mod seams;

/// The accumulator applies all three event kinds and rates are
/// delta-over-period between two reports.
#[test]
fn accumulator_applies_events_and_computes_rates() {
    let mut acc = MetricAccumulator::default();
    let t0 = Instant::now();

    let room0 = RoomSample {
        room: RoomId(1),
        emit_at: t0,
        steps: 30,
        budget_us: 33,
        dropped_frames: 2,
        sends_closed: 0,
        snap_bytes: 3_000,
        shipped_bytes: 30_000,
        lagged_events: 0,
        lagged_ticks: 0,
        step_min_us: 8,
        step_max_us: 900,
        step_sum_us: 420,
        step_hist: [20, 5, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        step_fine_hist: [0; FINE_HIST_BINS],
        late_min_us: 1,
        late_max_us: 2_000,
        late_sum_us: 300,
        keepalive_resends: 1,
        snapshots: 29,
        snapshots_withheld: 3,
        snap_bytes_max: 796,
        snap_overflows: 0,
        snap_records: 29,
        shipped_frames: 30,
        private_frames: 0,
        joins: 2,
        leaves: 0,
        detached: 0,
        resumes: 0,
        resume_rejected_stale: 0,
        detach_expired_despawn: 0,
        detach_expired_ai: 0,
        detach_forced: 0,
        effects_applied: 0,
        effects_forwarded: 0,
        effects_orphaned: 0,
        effects_dropped: 0,
        effects_refused: 0,
        migrations_out: 0,
        migrations_in: 0,
        migrations_failed: 0,
        team_exports: 0,
        team_export_drops_full: 0,
        team_export_drops_closed: 0,
        team_export_records: 0,
        team_over_cap: 0,
        team_over_budget: 0,
        team_imports: 0,
        team_import_records: 0,
        team_expired: 0,
        requests_local: 0,
        requests_external: 0,
        requests_rejected_malformed: 0,
        requests_rejected_dup: 0,
        requests_rejected_no_handler: 0,
        requests_rejected_logic: 0,
        requests_rejected_conn_cap: 0,
        requests_rejected_room_cap: 0,
        requests_refused_congested: 0,
        requests_dropped_unread: 0,
        requests_dropped_unbound: 0,
        actions_dropped_unread: 0,
        actions_dropped_unbound: 0,
        requests_timed_out: 0,
        requests_late: 0,
        requests_undelivered: 0,
        requests_abandoned: 0,
        pending_requests: 0,
        groups: 1,
        members: 3,
        max_group: 3,
        metrics_dropped: 0,
        stop: StopCounts::default(),
        logic: LogicCounters::new(),
    };
    acc.apply(MetricsEvent::Room(room0));
    acc.apply(MetricsEvent::Registry(RegistrySample {
        rooms: 1,
        conns: 3,
        rooms_created: 1,
        rooms_destroyed: 0,
        rooms_died: 0,
        joins: 3,
        leaves: 0,
        opens: 3,
        closes: 0,
        metrics_dropped: 1,
        join_ops_dropped: 0,
        close_ops_dropped: 0,
        team_relays_dropped_full: 0,
        team_relays_dropped_closed: 0,
        joins_unread: 0,
        team_exports_unread: 0,
        unauth_source_moves_kept: 0,
    }));
    acc.apply(MetricsEvent::Conn(ConnSample {
        conn: ConnectionId(1),
        bytes_in: 100,
        bytes_out: 20,
        frames_in: 5,
        frames_out: 2,
        actions_dropped: 0,
        metrics_dropped: 2,
        violations: 0,
        input_rate_limited: 2,
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
        server_close: None,
        last: false,
        tickets: Default::default(),
    }));

    let first = acc.report(t0);
    assert_eq!(first.rooms.len(), 1);
    // First report: no previous window yet.
    assert_eq!(first.rooms[0].hz, 0.0);

    // Advance one second: 30 more steps, 2 more drops, more bytes. The
    // rate is over the sample interval (`emit_at`), so the second
    // sample carries `t1 = t0 + 1s`.
    let t1 = t0 + Duration::from_secs(1);
    acc.apply(MetricsEvent::Room(RoomSample {
        emit_at: t1,
        steps: 60,
        dropped_frames: 4,
        snap_bytes: 6_000,
        shipped_bytes: 60_000,
        shipped_frames: 75,
        private_frames: 15,
        ..room0
    }));
    acc.apply(MetricsEvent::Conn(ConnSample {
        conn: ConnectionId(1),
        bytes_in: 50,
        bytes_out: 10,
        frames_in: 2,
        frames_out: 1,
        actions_dropped: 7,
        metrics_dropped: 0,
        violations: 3,
        input_rate_limited: 4,
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
        server_close: None,
        last: true,
        tickets: Default::default(),
    }));

    let second = acc.report(t1);
    let r = &second.rooms[0];
    // Total metric-channel drops: room (0) + registry (1) + conn deltas
    // (2 + 0) = 3.
    assert_eq!(second.metrics_dropped, 3);
    assert!((r.hz - 30.0).abs() < 1e-6, "hz from delta over 1 s");
    assert!((r.dropped_s - 2.0).abs() < 1e-6, "drop rate from delta");
    assert!(
        (r.shipped_s - 30_000.0).abs() < 1e-6,
        "shipped rate from delta"
    );
    assert!(
        (r.snap_bytes_s - 3_000.0).abs() < 1e-6,
        "encode rate from delta"
    );
    assert_eq!(r.steps, 60);
    // The shipped FRAME counts are carried straight through (cumulative
    // counters, no rate): they were maintained by both actors and
    // reached no report at all until the round that added them here, so
    // the carry itself is what this pins. Not derivable from
    // `shipped_bytes` — 60_000 bytes in 75 frames is an 800-byte mean
    // frame, which is the datagram-transport question the byte rate
    // alone cannot answer.
    assert_eq!(r.shipped_frames, 75, "shipped frames reach the report");
    assert_eq!(
        r.private_frames, 15,
        "and the private share of them stays distinguishable"
    );
    assert_eq!(r.step_mean_us, 420.0 / 60.0);
    assert_eq!(r.members, 3);
    assert_eq!(second.registry.unwrap().conns, 3);
    assert_eq!(second.net.bytes_in, 150);
    assert_eq!(second.net.bytes_out_control, 30);
    assert_eq!(second.net.bytes_out_room, 60_000);
    assert_eq!(second.net.bytes_out_total, 60_030);

    // Per-connection input-drop attribution: the only dropping sender
    // is c1 (7 actions total). Its sample carried `last: true`, so
    // the drops fold into the cumulative net total (monotonic) and
    // the per-connection entry retires with the connection — a
    // closed peer cannot flood again, so it leaves the top list.
    assert_eq!(second.net.actions_dropped, 7);
    assert!(
        second.actions_dropped_top.is_empty(),
        "a closed connection's attribution is retired, not listed"
    );
    // Violation events sum across the conn samples (0 + 3).
    assert_eq!(second.net.violations, 3);
    // Rate-limited input sums the same way (2 + 4), on its own field:
    // an over-rate honest client is not a violator (E1).
    assert_eq!(second.net.input_rate_limited, 6);

    // The render is one line per scope and parseable key=value. No
    // attribution line: nothing was dropped by a LIVE connection.
    let lines = second.render();
    assert_eq!(lines.len(), 4);
    assert!(lines[3].starts_with("gsb-metric scope=transport "));
    assert!(lines[0].starts_with("gsb-metric scope=registry "));
    assert!(lines[1].starts_with("gsb-metric scope=room id=r1 "));
    assert!(lines[2].starts_with("gsb-metric scope=net "));
    assert!(lines[2].contains("actions_dropped=7"));
    assert!(lines[2].contains(" violations=3 input_rate_limited=6 "));
    for line in &lines {
        for kv in line.split_whitespace().skip(2) {
            assert!(kv.contains('='), "key=value field: {kv}");
        }
    }
}

/// A minimal `RoomSample` for the pruning tests below (all counters
/// zero except `steps`).
pub(in crate::metrics) fn room_sample(room: RoomId, emit_at: Instant, steps: u64) -> RoomSample {
    RoomSample {
        room,
        emit_at,
        steps,
        budget_us: 33_333,
        dropped_frames: 0,
        sends_closed: 0,
        snap_bytes: 0,
        shipped_bytes: 0,
        lagged_events: 0,
        lagged_ticks: 0,
        step_min_us: 0,
        step_max_us: 0,
        step_sum_us: 0,
        step_hist: [0; HIST_BINS],
        step_fine_hist: [0; FINE_HIST_BINS],
        late_min_us: 0,
        late_max_us: 0,
        late_sum_us: 0,
        keepalive_resends: 0,
        snapshots: 0,
        snapshots_withheld: 0,
        snap_bytes_max: 0,
        snap_overflows: 0,
        snap_records: 0,
        shipped_frames: 0,
        private_frames: 0,
        joins: 0,
        leaves: 0,
        detached: 0,
        resumes: 0,
        resume_rejected_stale: 0,
        detach_expired_despawn: 0,
        detach_expired_ai: 0,
        detach_forced: 0,
        effects_applied: 0,
        effects_forwarded: 0,
        effects_orphaned: 0,
        effects_dropped: 0,
        effects_refused: 0,
        migrations_out: 0,
        migrations_in: 0,
        migrations_failed: 0,
        team_exports: 0,
        team_export_drops_full: 0,
        team_export_drops_closed: 0,
        team_export_records: 0,
        team_over_cap: 0,
        team_over_budget: 0,
        team_imports: 0,
        team_import_records: 0,
        team_expired: 0,
        requests_local: 0,
        requests_external: 0,
        requests_rejected_malformed: 0,
        requests_rejected_dup: 0,
        requests_rejected_no_handler: 0,
        requests_rejected_logic: 0,
        requests_rejected_conn_cap: 0,
        requests_rejected_room_cap: 0,
        requests_refused_congested: 0,
        requests_dropped_unread: 0,
        requests_dropped_unbound: 0,
        actions_dropped_unread: 0,
        actions_dropped_unbound: 0,
        requests_timed_out: 0,
        requests_late: 0,
        requests_undelivered: 0,
        requests_abandoned: 0,
        pending_requests: 0,
        groups: 0,
        members: 0,
        max_group: 0,
        metrics_dropped: 0,
        stop: StopCounts::default(),
        logic: LogicCounters::new(),
    }
}

/// Test room logic for the flow test below: one byte per tick per
/// group (declares "changed" every tick — a permitted, if wasteful,
/// logic) so the fan-out runs and a full out channel produces drops.
struct AlwaysLogic;

// Faz 1 trait split: shared contract on `GameLogic`; no room-exclusive
// hook used (empty `RoomLogic` impl).
impl crate::room::GameLogic<()> for AlwaysLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7200
    }
    fn private_op(&self) -> u16 {
        0x7201
    }
    fn group_of(&self, _w: &(), _p: crate::id::PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &crate::room::TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        out.extend_from_slice(b"x");
        true
    }
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> crate::room::Admission {
        crate::room::Admission {
            player: crate::id::PlayerId(c.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: crate::id::PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &crate::room::TickCtx, a: &mut Vec<crate::room::Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &crate::room::TickCtx) {}
}

impl crate::room::RoomLogic<()> for AlwaysLogic {}
