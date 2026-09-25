//! The Prometheus text exposition the ops surface serves at /metrics:
//! one HELP/TYPE pair per family, room ids as labels.

use crate::metrics::*;

mod closes;

impl MetricReport {
    /// Render as Prometheus text exposition format, version 0.0.4 (the
    /// scrape format of `/metrics`; see `docs/OPS.md` §3/§5): a
    /// `# HELP` / `# TYPE` header pair per metric family followed by its
    /// sample lines.
    ///
    /// Naming follows OPS §3's `<scope>_<counter>` vocabulary with the
    /// `gsb_` prefix and `*_total` on every counter; per-room samples
    /// carry the room as a LABEL (`gsb_room_steps_total{room="r1"}`) rather
    /// than embedding the id in the metric NAME (`gsb_room_r1_steps_total`):
    /// label is the canonical exposition practice — an id inside the name
    /// would mint one unbounded metric-name series per room per scrape,
    /// while the label keeps one family per counter and lets consumers
    /// aggregate or filter identically.
    ///
    /// Distributions: the budget-relative log2 step histogram exports as a
    /// classic histogram (`*_bucket` with cumulative counts, `le` in real µs
    /// derived from each room's `budget_us`, plus `_sum`/`_count`); the fine
    /// fixed-bin histogram — whose whole design purpose is sub-budget
    /// percentile resolution (see [`FINE_HIST_BINS`]) — exports as a summary
    /// (p50/p99 via [`fine_hist_percentile_us`]). A quantile whose rank
    /// falls beyond the fine histogram's cap has no fine value; the line is
    /// omitted rather than guessed (the log2 histogram still carries the
    /// overflow signal).
    pub fn render_prometheus(&self) -> String {
        use std::fmt::Write as _;

        /// Format a float as a Prometheus sample value (finite values print
        /// plainly; non-finite ones use the format's literals).
        fn val(v: f64) -> String {
            if v.is_nan() {
                "NaN".to_owned()
            } else if v.is_infinite() {
                if v > 0.0 {
                    "+Inf".to_owned()
                } else {
                    "-Inf".to_owned()
                }
            } else {
                format!("{v}")
            }
        }

        let mut out = String::with_capacity(4096);

        // Top-level: metric-channel health.
        out.push_str("# HELP gsb_metrics_dropped_total Metric samples dropped on the bounded metrics channel across all producers.\n");
        out.push_str("# TYPE gsb_metrics_dropped_total counter\n");
        let _ = writeln!(out, "gsb_metrics_dropped_total {}", self.metrics_dropped);

        // Registry scope (control-plane gauges + cumulative counters).
        if let Some(reg) = &self.registry {
            for (name, kind, help, v) in [
                (
                    "gsb_registry_rooms",
                    "gauge",
                    "Current room count.",
                    u64::from(reg.rooms),
                ),
                (
                    "gsb_registry_conns",
                    "gauge",
                    "Current registered connection count.",
                    u64::from(reg.conns),
                ),
                (
                    "gsb_registry_rooms_created_total",
                    "counter",
                    "Rooms created, cumulative.",
                    reg.rooms_created,
                ),
                (
                    "gsb_registry_rooms_destroyed_total",
                    "counter",
                    "Rooms destroyed, cumulative.",
                    reg.rooms_destroyed,
                ),
                (
                    "gsb_registry_rooms_died_total",
                    "counter",
                    "Rooms/shards died unexpectedly (panicked), cumulative.",
                    reg.rooms_died,
                ),
                (
                    "gsb_registry_joins_total",
                    "counter",
                    "Joins completed, cumulative.",
                    reg.joins,
                ),
                (
                    "gsb_registry_leaves_total",
                    "counter",
                    "Leaves completed, cumulative.",
                    reg.leaves,
                ),
                (
                    "gsb_registry_opens_total",
                    "counter",
                    "Connections opened, cumulative.",
                    reg.opens,
                ),
                (
                    "gsb_registry_closes_total",
                    "counter",
                    "Connections closed, cumulative.",
                    reg.closes,
                ),
            ] {
                out.push_str("# HELP ");
                out.push_str(name);
                out.push(' ');
                out.push_str(help);
                out.push('\n');
                let _ = write!(out, "# TYPE {name} {kind}\n{name} {v}\n");
            }
        }

        // Net scope: wire traffic aggregated over all connections (the
        // per-connection attribution lives in actions_dropped_top /
        // per-actor log lines, not in this aggregate).
        let n = &self.net;
        for (name, help, v) in [
            (
                "gsb_net_bytes_in_total",
                "Wire bytes received over all connections (frame bodies), cumulative.",
                n.bytes_in,
            ),
            (
                "gsb_net_bytes_out_room_total",
                "Snapshot+private payload bytes shipped by rooms, cumulative.",
                n.bytes_out_room,
            ),
            (
                "gsb_net_bytes_out_control_total",
                "Control-frame bytes sent by connection actors, cumulative.",
                n.bytes_out_control,
            ),
            (
                "gsb_net_bytes_out_total",
                "Total wire bytes sent, cumulative.",
                n.bytes_out_total,
            ),
            (
                "gsb_net_frames_in_total",
                "Frames received over all connections, cumulative.",
                n.frames_in,
            ),
            (
                "gsb_net_frames_out_total",
                "Control frames sent by connection actors, cumulative.",
                n.frames_out,
            ),
            (
                "gsb_net_actions_dropped_total",
                "Input actions dropped on full per-connection action channels, cumulative.",
                n.actions_dropped,
            ),
            (
                "gsb_net_violations_total",
                "Protocol-violation events counted by violation budgets, cumulative.",
                n.violations,
            ),
        ] {
            out.push_str("# HELP ");
            out.push_str(name);
            out.push(' ');
            out.push_str(help);
            out.push('\n');
            let _ = write!(out, "# TYPE {name} counter\n{name} {v}\n");
        }
        closes::render(&mut out, &n.server_closes);

        let rooms = &self.rooms;
        if rooms.is_empty() {
            return out;
        }

        // Helper: one labeled-per-room family. Header once, then a sample
        // line per room (`room="r<id>"` label).
        fn family(
            out: &mut String,
            name: &str,
            kind: &str,
            help: &str,
            rooms: &[RoomReport],
            f: impl Fn(&RoomReport) -> f64,
        ) {
            out.push_str("# HELP ");
            out.push_str(name);
            out.push(' ');
            out.push_str(help);
            out.push('\n');
            let _ = writeln!(out, "# TYPE {name} {kind}");
            for r in rooms {
                let _ = writeln!(out, "{name}{{room=\"r{}\"}} {}", r.room.0, val(f(r)));
            }
        }
        fn counters(
            out: &mut String,
            name: &str,
            help: &str,
            rooms: &[RoomReport],
            f: impl Fn(&RoomReport) -> u64,
        ) {
            family(out, name, "counter", help, rooms, |r| f(r) as f64);
        }
        fn gauges(
            out: &mut String,
            name: &str,
            help: &str,
            rooms: &[RoomReport],
            f: impl Fn(&RoomReport) -> f64,
        ) {
            family(out, name, "gauge", help, rooms, f);
        }

        // Tick health.
        gauges(
            &mut out,
            "gsb_room_hz",
            "Measured room step rate (Δsteps/s over the last sample interval).",
            rooms,
            |r| r.hz,
        );
        gauges(
            &mut out,
            "gsb_room_budget_us",
            "The room's tick budget in microseconds (one period).",
            rooms,
            |r| r.budget_us as f64,
        );
        gauges(
            &mut out,
            "gsb_room_step_min_us",
            "Minimum step body duration, µs.",
            rooms,
            |r| r.step_min_us as f64,
        );
        gauges(
            &mut out,
            "gsb_room_step_mean_us",
            "Mean step body duration, µs.",
            rooms,
            |r| r.step_mean_us,
        );
        gauges(
            &mut out,
            "gsb_room_step_max_us",
            "Maximum step body duration, µs.",
            rooms,
            |r| r.step_max_us as f64,
        );
        gauges(
            &mut out,
            "gsb_room_late_min_us",
            "Minimum tick processing latency (step start − ticker timestamp), µs.",
            rooms,
            |r| r.late_min_us as f64,
        );
        gauges(
            &mut out,
            "gsb_room_late_mean_us",
            "Mean tick processing latency, µs.",
            rooms,
            |r| r.late_mean_us,
        );
        gauges(
            &mut out,
            "gsb_room_late_max_us",
            "Maximum tick processing latency, µs.",
            rooms,
            |r| r.late_max_us as f64,
        );

        // Step-duration distribution #1: budget-relative log2 histogram,
        // exported with REAL microsecond bucket edges (derived per room from
        // budget_us so the scrape reads in absolute units) and cumulative
        // counts, as the histogram format requires.
        out.push_str("# HELP gsb_room_step_hist Step body duration histogram; buckets are fractions of the room's tick budget (le in µs); bins at/above the budget edge are budget overflow.\n");
        out.push_str("# TYPE gsb_room_step_hist histogram\n");
        for r in rooms {
            let mut cum = 0u64;
            for (i, &cnt) in r.step_hist.iter().enumerate() {
                cum += cnt;
                let le = if i < HIST_EDGES.len() {
                    hist_edge_us(r.budget_us, i).to_string()
                } else {
                    "+Inf".to_owned()
                };
                let _ = writeln!(
                    out,
                    "gsb_room_step_hist_bucket{{room=\"r{}\",le=\"{}\"}} {cum}",
                    r.room.0, le
                );
            }
            // `_sum` in µs: RoomReport carries mean (sum/steps by
            // construction) and steps, so mean × steps reconstructs the
            // exact total duration without widening the report.
            let _ = writeln!(
                out,
                "gsb_room_step_hist_sum{{room=\"r{}\"}} {}",
                r.room.0,
                val(r.step_mean_us * r.steps as f64)
            );
            let _ = writeln!(
                out,
                "gsb_room_step_hist_count{{room=\"r{}\"}} {}",
                r.room.0,
                r.step_hist.iter().sum::<u64>()
            );
        }

        // Step-duration distribution #2: the fine fixed-bin histogram as a
        // p50/p99 summary (its design purpose — sub-budget resolution).
        out.push_str("# HELP gsb_room_step_duration_us Step body duration, µs (summary quantiles from the fine fixed-bin histogram; steps at/above its cap are not quantiled here).\n");
        out.push_str("# TYPE gsb_room_step_duration_us summary\n");
        for r in rooms {
            for (q, p) in [("0.5", 50u32), ("0.99", 99)] {
                if let Some(us) = fine_hist_percentile_us(&r.step_fine_hist, r.steps, p) {
                    let _ = writeln!(
                        out,
                        "gsb_room_step_duration_us{{room=\"r{}\",quantile=\"{q}\"}} {us}",
                        r.room.0
                    );
                }
            }
        }

        // Drops and broadcast fan-out.
        counters(
            &mut out,
            "gsb_room_steps_total",
            "Steps run, cumulative.",
            rooms,
            |r| r.steps,
        );
        counters(
            &mut out,
            "gsb_room_lagged_events_total",
            "Broadcast Lagged occurrences, cumulative.",
            rooms,
            |r| r.lagged_events,
        );
        counters(
            &mut out,
            "gsb_room_lagged_ticks_total",
            "Missed tick indices caught up, cumulative.",
            rooms,
            |r| r.lagged_ticks,
        );
        counters(
            &mut out,
            "gsb_room_dropped_total",
            "Outbound batches dropped at the fan-out (slow client), cumulative.",
            rooms,
            |r| r.dropped,
        );
        gauges(
            &mut out,
            "gsb_room_dropped_s",
            "Batch drop rate (Δ/s over the last sample interval).",
            rooms,
            |r| r.dropped_s,
        );
        counters(
            &mut out,
            "gsb_room_keepalive_resends_total",
            "Keep-alive re-sends of unchanged groups, cumulative.",
            rooms,
            |r| r.keepalive_resends,
        );
        counters(
            &mut out,
            "gsb_room_snapshots_total",
            "Group snapshots encoded, cumulative.",
            rooms,
            |r| r.snapshots,
        );
        gauges(
            &mut out,
            "gsb_room_snap_bytes_s",
            "Encoded snapshot byte rate (Δ/s).",
            rooms,
            |r| r.snap_bytes_s,
        );
        gauges(
            &mut out,
            "gsb_room_snap_bytes_max",
            "Largest single group payload seen, bytes.",
            rooms,
            |r| f64::from(r.snap_bytes_max),
        );
        counters(
            &mut out,
            "gsb_room_snap_overflows_total",
            "Snapshots exceeding max_snapshot_bytes, cumulative.",
            rooms,
            |r| r.snap_overflows,
        );
        counters(
            &mut out,
            "gsb_room_snap_records_total",
            "Entity records encoded (overlap-metric numerator), cumulative.",
            rooms,
            |r| r.snap_records,
        );
        counters(
            &mut out,
            "gsb_room_shipped_bytes_total",
            "Bytes shipped to connections (fan-out copies), cumulative.",
            rooms,
            |r| r.shipped_bytes,
        );
        gauges(
            &mut out,
            "gsb_room_shipped_s",
            "Shipped-byte rate (Δ/s).",
            rooms,
            |r| r.shipped_s,
        );

        // Group/membership state.
        gauges(
            &mut out,
            "gsb_room_groups",
            "Current snapshot group count.",
            rooms,
            |r| f64::from(r.groups),
        );
        gauges(
            &mut out,
            "gsb_room_members",
            "Current member count.",
            rooms,
            |r| f64::from(r.members),
        );
        gauges(
            &mut out,
            "gsb_room_max_group",
            "Largest snapshot group right now.",
            rooms,
            |r| f64::from(r.max_group),
        );
        gauges(
            &mut out,
            "gsb_room_detached",
            "Detached-but-parked connections right now (their cap slots are held).",
            rooms,
            |r| f64::from(r.detached),
        );

        // Session lifecycle.
        counters(
            &mut out,
            "gsb_room_joins_total",
            "Joins processed, cumulative.",
            rooms,
            |r| r.joins,
        );
        counters(
            &mut out,
            "gsb_room_leaves_total",
            "Leaves processed, cumulative.",
            rooms,
            |r| r.leaves,
        );
        counters(
            &mut out,
            "gsb_room_resumes_total",
            "Resumes accepted onto parked sessions, cumulative.",
            rooms,
            |r| r.resumes,
        );
        counters(
            &mut out,
            "gsb_room_resume_rejected_stale_total",
            "Resume attempts rejected as stale, cumulative.",
            rooms,
            |r| r.resume_rejected_stale,
        );
        counters(
            &mut out,
            "gsb_room_detach_expired_despawn_total",
            "Detach holds expired toward despawn, cumulative.",
            rooms,
            |r| r.detach_expired_despawn,
        );
        counters(
            &mut out,
            "gsb_room_detach_expired_ai_total",
            "Detach holds expired toward AI handover, cumulative.",
            rooms,
            |r| r.detach_expired_ai,
        );

        // The detach-hold ceiling, remote effects, migrations and the team
        // exchange (the last three families: shard rows only — 0 on a
        // single room). One table so the names and helps read side by side.
        type Row = (&'static str, &'static str, fn(&RoomReport) -> u64);
        let rows: [Row; 16] = [
            (
                "gsb_room_detach_forced_total",
                "Detach holds forced to end by max_detach_hold over a standing veto, cumulative.",
                |r| r.detach_forced,
            ),
            (
                "gsb_room_effects_applied_total",
                "Remote effects this shard's game applied as their authority, cumulative.",
                |r| r.effects_applied,
            ),
            (
                "gsb_room_effects_forwarded_total",
                "Remote effects handed on to a migrated target's new owner, cumulative.",
                |r| r.effects_forwarded,
            ),
            (
                "gsb_room_effects_orphaned_total",
                "Remote effects whose target is gone, cumulative.",
                |r| r.effects_orphaned,
            ),
            (
                "gsb_room_effects_dropped_total",
                "Remote effects lost to a full retry buffer, a closed link, the hop or the age bound, cumulative.",
                |r| r.effects_dropped,
            ),
            (
                "gsb_room_effects_refused_total",
                "Remote-effect emits refused at the source (budget spent, target not lent), cumulative.",
                |r| r.effects_refused,
            ),
            (
                "gsb_room_migrations_out_total",
                "Entities handed to a neighbour shard (committed sends), cumulative.",
                |r| r.migrations_out,
            ),
            (
                "gsb_room_migrations_in_total",
                "Entities installed from a neighbour shard, cumulative.",
                |r| r.migrations_in,
            ),
            (
                "gsb_room_migrations_failed_total",
                "Migration sends a full neighbour inbox refused (retried next tick), cumulative.",
                |r| r.migrations_failed,
            ),
            (
                "gsb_room_team_exports_total",
                "Team exports queued on the registry's mailbox (the team hub), cumulative.",
                |r| r.team_exports,
            ),
            (
                "gsb_room_team_export_drops_total",
                "Team exports a full or closed registry mailbox refused, cumulative.",
                |r| r.team_export_drops,
            ),
            (
                "gsb_room_team_export_records_total",
                "Records in the queued team exports, cumulative.",
                |r| r.team_export_records,
            ),
            (
                "gsb_room_team_over_cap_total",
                "Team records (and viewed teams) cut by the core's per-message caps, cumulative.",
                |r| r.team_over_cap,
            ),
            (
                "gsb_room_team_imports_total",
                "Team imports (the hub's relays) applied, cumulative.",
                |r| r.team_imports,
            ),
            (
                "gsb_room_team_import_records_total",
                "Records in the applied team imports, cumulative.",
                |r| r.team_import_records,
            ),
            (
                "gsb_room_team_expired_total",
                "Team import slots dropped by the TTL (a silent source), cumulative.",
                |r| r.team_expired,
            ),
        ];
        for (name, help, get) in rows {
            counters(&mut out, name, help, rooms, get);
        }

        // RPC.
        counters(
            &mut out,
            "gsb_room_requests_local_total",
            "RPC requests answered room-local, cumulative.",
            rooms,
            |r| r.requests_local,
        );
        counters(
            &mut out,
            "gsb_room_requests_external_total",
            "RPC requests delegated to workers, cumulative.",
            rooms,
            |r| r.requests_external,
        );
        counters(
            &mut out,
            "gsb_room_requests_rejected_malformed_total",
            "RPC rejections: malformed envelope/id=0, cumulative.",
            rooms,
            |r| r.requests_rejected_malformed,
        );
        counters(
            &mut out,
            "gsb_room_requests_rejected_dup_total",
            "RPC rejections: duplicate in-flight id, cumulative.",
            rooms,
            |r| r.requests_rejected_dup,
        );
        counters(
            &mut out,
            "gsb_room_requests_rejected_no_handler_total",
            "RPC rejections: no handler for the op, cumulative.",
            rooms,
            |r| r.requests_rejected_no_handler,
        );
        counters(
            &mut out,
            "gsb_room_requests_rejected_logic_total",
            "RPC rejections: the logic's own reject decision, cumulative.",
            rooms,
            |r| r.requests_rejected_logic,
        );
        counters(
            &mut out,
            "gsb_room_requests_rejected_conn_cap_total",
            "RPC rejections: per-connection pending cap, cumulative.",
            rooms,
            |r| r.requests_rejected_conn_cap,
        );
        counters(
            &mut out,
            "gsb_room_requests_rejected_room_cap_total",
            "RPC rejections: room-wide pending cap, cumulative.",
            rooms,
            |r| r.requests_rejected_room_cap,
        );
        counters(
            &mut out,
            "gsb_room_requests_timed_out_total",
            "RPC pending requests swept as timed out, cumulative.",
            rooms,
            |r| r.requests_timed_out,
        );
        counters(
            &mut out,
            "gsb_room_requests_late_total",
            "RPC worker reports dropped as late, cumulative.",
            rooms,
            |r| r.requests_late,
        );
        gauges(
            &mut out,
            "gsb_room_pending_requests",
            "External RPC requests currently in flight.",
            rooms,
            |r| f64::from(r.pending_requests),
        );
        counters(
            &mut out,
            "gsb_room_metrics_dropped_total",
            "Metric samples this room dropped on a full metrics channel, cumulative.",
            rooms,
            |r| r.metrics_dropped,
        );

        out
    }
}
