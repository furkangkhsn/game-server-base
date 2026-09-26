//! Folding the accumulated samples into one [`MetricReport`]: the
//! rates are timed against each sample's own interval, never the
//! report window.
use std::time::Instant;

use super::MetricAccumulator;
use crate::id::{ConnectionId, RoomId};
use crate::metrics::*;

impl MetricAccumulator {
    /// Snapshot the accumulated state as a report and advance the rate
    /// window. Per-room rates are Δ since the previous sample, over the
    /// sample interval (see `RoomAcc::latest`); `at` (the report time)
    /// is kept for the caller's bookkeeping but no longer drives the rates.
    pub fn report(&mut self, at: Instant) -> MetricReport {
        let mut rooms = Vec::with_capacity(self.rooms.len());
        for (id, acc) in &mut self.rooms {
            let latest = acc.latest;
            // Rate window = the SAMPLE interval (this sample's `emit_at`
            // minus the previous sample's), not the report window — see
            // `RoomSample::emit_at`.
            let (hz, dropped_s, snap_bytes_s, shipped_s) = match acc.prev {
                Some(p) => {
                    let dt = latest
                        .emit_at
                        .saturating_duration_since(p.emit_at)
                        .as_secs_f64()
                        .max(1e-9);
                    (
                        (latest.steps as f64 - p.steps as f64) / dt,
                        (latest.dropped_frames as f64 - p.dropped_frames as f64) / dt,
                        (latest.snap_bytes as f64 - p.snap_bytes as f64) / dt,
                        (latest.shipped_bytes as f64 - p.shipped_bytes as f64) / dt,
                    )
                }
                None => (0.0, 0.0, 0.0, 0.0),
            };
            let steps = latest.steps.max(1);
            rooms.push(RoomReport {
                room: *id,
                steps: latest.steps,
                hz,
                budget_us: latest.budget_us,
                step_min_us: latest.step_min_us,
                step_mean_us: latest.step_sum_us as f64 / steps as f64,
                step_max_us: latest.step_max_us,
                step_hist: latest.step_hist,
                step_fine_hist: latest.step_fine_hist.map(u64::from),
                late_min_us: latest.late_min_us,
                late_mean_us: latest.late_sum_us as f64 / steps as f64,
                late_max_us: latest.late_max_us,
                lagged_events: latest.lagged_events,
                lagged_ticks: latest.lagged_ticks,
                dropped: latest.dropped_frames,
                dropped_s,
                keepalive_resends: latest.keepalive_resends,
                snapshots: latest.snapshots,
                snap_bytes_s,
                snap_bytes_max: latest.snap_bytes_max,
                snap_overflows: latest.snap_overflows,
                snap_records: latest.snap_records,
                shipped_bytes: latest.shipped_bytes,
                shipped_s,
                shipped_frames: latest.shipped_frames,
                private_frames: latest.private_frames,
                groups: latest.groups,
                members: latest.members,
                max_group: latest.max_group,
                joins: latest.joins,
                leaves: latest.leaves,
                detached: latest.detached,
                resumes: latest.resumes,
                resume_rejected_stale: latest.resume_rejected_stale,
                detach_expired_despawn: latest.detach_expired_despawn,
                detach_expired_ai: latest.detach_expired_ai,
                detach_forced: latest.detach_forced,
                effects_applied: latest.effects_applied,
                effects_forwarded: latest.effects_forwarded,
                effects_orphaned: latest.effects_orphaned,
                effects_dropped: latest.effects_dropped,
                effects_refused: latest.effects_refused,
                migrations_out: latest.migrations_out,
                migrations_in: latest.migrations_in,
                migrations_failed: latest.migrations_failed,
                team_exports: latest.team_exports,
                team_export_drops: latest.team_export_drops,
                team_export_records: latest.team_export_records,
                team_over_cap: latest.team_over_cap,
                team_over_budget: latest.team_over_budget,
                team_imports: latest.team_imports,
                team_import_records: latest.team_import_records,
                team_expired: latest.team_expired,
                requests_local: latest.requests_local,
                requests_external: latest.requests_external,
                requests_rejected_malformed: latest.requests_rejected_malformed,
                requests_rejected_dup: latest.requests_rejected_dup,
                requests_rejected_no_handler: latest.requests_rejected_no_handler,
                requests_rejected_logic: latest.requests_rejected_logic,
                requests_rejected_conn_cap: latest.requests_rejected_conn_cap,
                requests_rejected_room_cap: latest.requests_rejected_room_cap,
                requests_timed_out: latest.requests_timed_out,
                requests_late: latest.requests_late,
                pending_requests: latest.pending_requests,
                metrics_dropped: latest.metrics_dropped,
                logic: latest.logic,
            });
            acc.prev = Some(latest);
        }
        let bytes_out_room: u64 = rooms.iter().map(|r| r.shipped_bytes).sum();
        // Total metric-channel drops: room (cumulative, latest per room) +
        // registry (cumulative, latest) + connection actors (delta, summed).
        let metrics_dropped = rooms
            .iter()
            .map(|r| r.metrics_dropped)
            .sum::<u64>()
            .saturating_add(self.registry.map(|r| r.metrics_dropped).unwrap_or(0))
            .saturating_add(self.conn_metrics_dropped);
        // Per-connection input-drop attribution: worst offenders first
        // (count desc, connection id asc as the deterministic tie-break).
        // The cumulative total folds in the drops RETIRED with closed
        // connections, so `net.actions_dropped` stays monotonic even
        // though the map only names live connections.
        let actions_dropped_total: u64 = self
            .conn_actions_dropped
            .values()
            .sum::<u64>()
            .saturating_add(self.conn_actions_dropped_retired);
        let mut actions_dropped_top: Vec<(ConnectionId, u64)> = self
            .conn_actions_dropped
            .iter()
            .map(|(c, n)| (*c, *n))
            .collect();
        actions_dropped_top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        actions_dropped_top.truncate(5);
        // Age the destroyed-room linger windows: each emitted report burns
        // one (see ROOM_GONE_GRACE_REPORTS); when a room's windows run
        // out, its accumulator finally goes.
        let expired: Vec<RoomId> = self
            .rooms_gone_grace
            .iter()
            .filter(|(_, left)| **left == 1)
            .map(|(id, _)| *id)
            .collect();
        for left in self.rooms_gone_grace.values_mut() {
            *left -= 1;
        }
        self.rooms_gone_grace.retain(|_, left| *left > 0);
        for id in expired {
            self.rooms.remove(&id);
        }
        MetricReport {
            metrics_dropped,
            emitted_at: at,
            registry: self.registry.map(|r| RegistryReport {
                rooms: r.rooms,
                conns: r.conns,
                rooms_created: r.rooms_created,
                rooms_destroyed: r.rooms_destroyed,
                rooms_died: r.rooms_died,
                joins: r.joins,
                leaves: r.leaves,
                opens: r.opens,
                closes: r.closes,
            }),
            // (registry report intentionally carries no metrics_dropped: the
            // registry's drop count is cumulative in its sample and is folded
            // into the top-level `metrics_dropped` above.)
            net: NetReport {
                bytes_in: self.conn_bytes_in,
                bytes_out_room,
                bytes_out_control: self.conn_bytes_out,
                bytes_out_total: bytes_out_room.saturating_add(self.conn_bytes_out),
                frames_in: self.conn_frames_in,
                frames_out: self.conn_frames_out,
                actions_dropped: actions_dropped_total,
                violations: self.conn_violations,
                server_closes: self.conn_server_closes,
            },
            actions_dropped_top,
            rooms,
        }
    }
}
