//! The one-line human/log rendering of a report (the Prometheus
//! exposition is the sibling `prometheus` module).

use crate::metrics::*;

impl MetricReport {
    /// Render as one parseable `key=value` line per scope (stable format:
    /// the load generator and operators grep these).
    pub fn render(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(r) = &self.registry {
            lines.push(format!(
                "gsb-metric scope=registry rooms={} conns={} opens={} closes={} \
                 joins={} leaves={} rooms_created={} rooms_destroyed={} rooms_died={}",
                r.rooms,
                r.conns,
                r.opens,
                r.closes,
                r.joins,
                r.leaves,
                r.rooms_created,
                r.rooms_destroyed,
                r.rooms_died
            ));
        }
        for r in &self.rooms {
            lines.push(format!(
                "gsb-metric scope=room id={} steps={} hz={:.2} \
                 step_budget_us={} step_min_us={} step_mean_us={:.1} step_max_us={} \
                 step_hist=[{}] \
                 late_min_us={} late_mean_us={:.1} late_max_us={} \
                 lagged_events={} lagged_ticks={} dropped={} dropped_s={:.1} \
                 keepalive_resends={} snapshots={} \
                 snap_bytes_s={:.0} snap_bytes_max={} snap_overflows={} \
                 snap_records={} \
                 shipped_bytes={} shipped_s={:.0} \
                 shipped_frames={} private_frames={} \
                 groups={} members={} max_group={} joins={} leaves={} \
                 detached={} resumes={} resume_rejected_stale={} \
                 detach_expired_despawn={} detach_expired_ai={} \
                 req_local={} req_ext={} \
                 req_rej_malformed={} req_rej_dup={} req_rej_no_handler={} \
                 req_rej_logic={} req_rej_conn={} req_rej_room={} \
                 req_to={} req_late={} \
                 req_pending={} metrics_dropped={}",
                r.room,
                r.steps,
                r.hz,
                r.budget_us,
                r.step_min_us,
                r.step_mean_us,
                r.step_max_us,
                r.step_hist
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                r.late_min_us,
                r.late_mean_us,
                r.late_max_us,
                r.lagged_events,
                r.lagged_ticks,
                r.dropped,
                r.dropped_s,
                r.keepalive_resends,
                r.snapshots,
                r.snap_bytes_s,
                r.snap_bytes_max,
                r.snap_overflows,
                r.snap_records,
                r.shipped_bytes,
                r.shipped_s,
                r.shipped_frames,
                r.private_frames,
                r.groups,
                r.members,
                r.max_group,
                r.joins,
                r.leaves,
                r.detached,
                r.resumes,
                r.resume_rejected_stale,
                r.detach_expired_despawn,
                r.detach_expired_ai,
                r.requests_local,
                r.requests_external,
                r.requests_rejected_malformed,
                r.requests_rejected_dup,
                r.requests_rejected_no_handler,
                r.requests_rejected_logic,
                r.requests_rejected_conn_cap,
                r.requests_rejected_room_cap,
                r.requests_timed_out,
                r.requests_late,
                r.pending_requests,
                r.metrics_dropped
            ));
        }
        let n = &self.net;
        lines.push(format!(
            "gsb-metric scope=net bytes_in={} bytes_out_room={} \
             bytes_out_control={} bytes_out_total={} frames_in={} frames_out={} \
             actions_dropped={} violations={} metrics_dropped={}",
            n.bytes_in,
            n.bytes_out_room,
            n.bytes_out_control,
            n.bytes_out_total,
            n.frames_in,
            n.frames_out,
            n.actions_dropped,
            n.violations,
            self.metrics_dropped
        ));
        if !self.actions_dropped_top.is_empty() {
            // Attribution of the net-scope `actions_dropped`: which
            // connection's own input was lost to its full action channel
            // (worst first; "c<id>:<count>", comma-joined).
            lines.push(format!(
                "gsb-metric scope=net actions_dropped_top={}",
                self.actions_dropped_top
                    .iter()
                    .map(|(c, n)| format!("{c}:{n}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        lines
    }
}
