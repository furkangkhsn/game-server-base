//! The one-line human/log rendering of a report (the Prometheus
//! exposition is the sibling `prometheus` module).

use std::fmt::Write as _;

use crate::metrics::*;

impl MetricReport {
    /// Render as one parseable `key=value` line per scope (stable format:
    /// the load generator and operators grep these).
    pub fn render(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(r) = &self.registry {
            lines.push(format!(
                "gsb-metric scope=registry rooms={} conns={} opens={} closes={} \
                 joins={} leaves={} rooms_created={} rooms_destroyed={} rooms_died={} \
                 join_ops_dropped={} close_ops_dropped={} \
                 match_results_dropped_full={} match_results_dropped_closed={} \
                 rooms_ended_uncounted={} \
                 team_relays_dropped_full={} team_relays_dropped_closed={}",
                r.rooms,
                r.conns,
                r.opens,
                r.closes,
                r.joins,
                r.leaves,
                r.rooms_created,
                r.rooms_destroyed,
                r.rooms_died,
                r.join_ops_dropped,
                r.close_ops_dropped,
                r.match_results_dropped_full,
                r.match_results_dropped_closed,
                r.rooms_ended_uncounted,
                r.team_relays_dropped_full,
                r.team_relays_dropped_closed
            ));
        }
        for r in &self.rooms {
            let mut line = format!(
                "gsb-metric scope=room id={} steps={} hz={:.2} \
                 step_budget_us={} step_min_us={} step_mean_us={:.1} step_max_us={} \
                 step_hist=[{}] \
                 late_min_us={} late_mean_us={:.1} late_max_us={} \
                 lagged_events={} lagged_ticks={} dropped={} dropped_s={:.1} \
                 sends_closed={} keepalive_resends={} snapshots={} \
                 snap_bytes_s={:.0} snap_bytes_max={} snap_overflows={} \
                 snap_records={} \
                 shipped_bytes={} shipped_s={:.0} \
                 shipped_frames={} private_frames={} \
                 groups={} members={} max_group={} joins={} leaves={} \
                 detached={} resumes={} resume_rejected_stale={} \
                 detach_expired_despawn={} detach_expired_ai={} detach_forced={} \
                 effects_applied={} effects_forwarded={} effects_orphaned={} \
                 effects_dropped={} effects_refused={} \
                 migrations_out={} migrations_in={} migrations_failed={} \
                 team_exports={} team_export_drops={} team_export_records={} \
                 team_over_cap={} team_over_budget={} team_imports={} \
                 team_import_records={} team_expired={} \
                 actions_unread={} actions_unbound={} \
                 req_local={} req_ext={} \
                 req_rej_malformed={} req_rej_dup={} req_rej_no_handler={} \
                 req_rej_logic={} req_rej_conn={} req_rej_room={} \
                 req_refused={} req_unread={} req_unbound={} req_to={} req_late={} \
                 req_undelivered={} req_abandoned={} \
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
                r.sends_closed,
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
                r.detach_forced,
                r.effects_applied,
                r.effects_forwarded,
                r.effects_orphaned,
                r.effects_dropped,
                r.effects_refused,
                r.migrations_out,
                r.migrations_in,
                r.migrations_failed,
                r.team_exports,
                r.team_export_drops,
                r.team_export_records,
                r.team_over_cap,
                r.team_over_budget,
                r.team_imports,
                r.team_import_records,
                r.team_expired,
                r.actions_dropped_unread,
                r.actions_dropped_unbound,
                r.requests_local,
                r.requests_external,
                r.requests_rejected_malformed,
                r.requests_rejected_dup,
                r.requests_rejected_no_handler,
                r.requests_rejected_logic,
                r.requests_rejected_conn_cap,
                r.requests_rejected_room_cap,
                r.requests_refused_congested,
                r.requests_dropped_unread,
                r.requests_dropped_unbound,
                r.requests_timed_out,
                r.requests_late,
                r.requests_undelivered,
                r.requests_abandoned,
                r.pending_requests,
                r.metrics_dropped
            );
            // What the stop found still held (B68): one stable key per
            // counter, zeros included (0 on every periodic sample).
            for (k, v) in r.stop.fields() {
                let _ = write!(line, " {k}={v}");
            }
            // The logic's own counters (F9), after every core key: one
            // `logic_<name>=<value>` each, in the order the logic put
            // them. Nothing at all for a logic that declares none.
            for s in r.logic.slots() {
                let _ = write!(line, " logic_{}={}", s.counter.name(), s.value);
            }
            // Then how many values the bound dropped (F17) — only while
            // there are any, so a logic within the bound keeps its line.
            if r.logic.dropped() > 0 {
                let _ = write!(line, " logic_counters_dropped={}", r.logic.dropped());
            }
            lines.push(line);
        }
        let n = &self.net;
        lines.push(format!(
            "gsb-metric scope=net bytes_in={} bytes_out_room={} \
             bytes_out_control={} bytes_out_total={} frames_in={} frames_out={} \
             actions_dropped={} violations={} input_rate_limited={} \
             actions_dropped_closed={} requests_dropped_closed={} \
             requests_dropped_full={} requests_no_room={} \
             hb_throttled_preauth={} hb_throttled_authed={} \
             frames_out_closed={} close_notices_dropped={} \
             requests_unprocessed={} actions_unprocessed={} \
             control_frames_unprocessed={} \
             metrics_dropped={} server_closes={}{}",
            n.bytes_in,
            n.bytes_out_room,
            n.bytes_out_control,
            n.bytes_out_total,
            n.frames_in,
            n.frames_out,
            n.actions_dropped,
            n.violations,
            n.input_rate_limited,
            n.actions_dropped_closed,
            n.requests_dropped_closed,
            n.requests_dropped_full,
            n.requests_no_room,
            n.heartbeats_throttled_preauth,
            n.heartbeats_throttled_authed,
            n.frames_out_closed,
            n.close_notices_dropped,
            n.requests_unprocessed,
            n.actions_unprocessed,
            n.control_frames_unprocessed,
            self.metrics_dropped,
            n.server_closes.total(),
            // One stable key per reason (`server_close_<reason>=N`, zeros
            // included), so a grep for one reason never depends on which
            // others happened to fire.
            n.server_closes
                .iter()
                .map(|(r, c)| format!(" server_close_{}={c}", r.label()))
                .collect::<String>()
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
        // The transport's own losses (B58): one stable key per counter,
        // zeros included, on a line of their own.
        let mut line = String::from("gsb-metric scope=transport");
        for (k, v) in self.transport.fields() {
            let _ = write!(line, " {k}={v}");
        }
        lines.push(line);
        lines
    }
}
