//! The report itself: the human-readable block and the single
//! scriptable RESULT line under it.

use super::*;
use gsb_core::metrics::{FINE_HIST_CAP_US, MetricReport};

pub(crate) fn print_report(
    args: &Args,
    inproc: bool,
    reports: &[ClientReport],
    hzs: &[f64],
    server_reports: &[MetricReport],
    sep: Option<&SepInfo>,
) {
    let mode = match (sep, inproc) {
        (Some(_), _) => "sep",
        (None, true) => "in-proc",
        (None, false) => "ext",
    };
    let mode_human = match (sep, inproc) {
        (Some(s), _) => format!(
            "separate processes (server isolated; affinity={}; server_cpu_s={:.1} clients_cpu_s={:.1})",
            s.affinity, s.server_cpu_s, s.clients_cpu_s
        ),
        (None, true) => "in-process (clients share CPU with server)".to_string(),
        (None, false) => "external server".to_string(),
    };
    // The report with the highest cumulative step count (cumulative
    // counters are monotonic, so this is the latest room state; the
    // shutdown emit carries the same cumulative values). For a sharded
    // room the shards step in lockstep, so the max over shards marks the
    // report's recency (see `report_steps`).
    let last_room = server_reports
        .iter()
        .filter(|r| !r.rooms.is_empty())
        .max_by_key(|r| report_steps(r));
    // The report's room(s) folded to one `RoomReport` (identity for a
    // single room; the cross-shard aggregate for a sharded room — see
    // `fold_rooms`).
    let last_room_agg = last_room.and_then(fold_rooms);
    // Peak registered connections across the whole run (the final report
    // is post-teardown, so its gauges are ~0).
    let peak_conns = server_reports
        .iter()
        .filter_map(|r| r.registry.map(|g| g.conns))
        .max()
        .unwrap_or(0);
    // Peak room membership (same rationale): the stable entity count for
    // the overlap ratio. For a sharded room this is the SUM over shards
    // (the room's total population — the shards partition its connections).
    let peak_members = server_reports
        .iter()
        .filter(|r| !r.rooms.is_empty())
        .map(report_members)
        .max()
        .unwrap_or(0);
    // The overlap measurement (D3): encoded entity records per tick in the
    // steady state, and per broadcastable entity (the multiplier). Both
    // endpoints are taken AFTER the join phase (base = first report with
    // >= 100 steps; the cumulative counters make the delta exact), so the
    // stagger's ramp-up is not in the window. The window's END is the last
    // report that still carries full population: in the separate-process
    // mode the server outlives the clients by a few seconds (clean stop
    // after the leave flushes), so the final report's window contains the
    // drain and would bias the delta low. `members` is a gauge (current
    // membership), so "members == peak_members" marks the steady reports.
    // (Sharded: the folded reports — summed members, summed records.)
    let base = server_reports
        .iter()
        .find(|r| !r.rooms.is_empty() && report_steps(r) >= 100)
        .and_then(fold_rooms);
    let last_steady = server_reports
        .iter()
        .filter(|r| !r.rooms.is_empty() && report_members(r) == peak_members)
        .max_by_key(|r| report_steps(r))
        .or(last_room)
        .and_then(fold_rooms);
    let rec_per_tick = match (base, last_steady) {
        (Some(b), Some(l)) if l.steps > b.steps => {
            let d_steps = l.steps - b.steps;
            let d_rec = l.snap_records.saturating_sub(b.snap_records);
            d_rec as f64 / d_steps as f64
        }
        _ => 0.0,
    };
    let overlap = if peak_members > 0 {
        rec_per_tick / peak_members as f64
    } else {
        0.0
    };
    // Stable measured server tick rate: the median over all reports'
    // per-window rates (excluding the first report's empty window and
    // any narrow shutdown window). For a sharded room the room's rate is
    // the SLOWEST shard's (the shards step together, so a lagging shard
    // drags the room).
    let server_hz = median(
        &server_reports
            .iter()
            .filter(|r| !r.rooms.is_empty())
            .map(|r| {
                r.rooms
                    .iter()
                    .map(|rm| rm.hz)
                    .filter(|h| *h > 0.0)
                    .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .unwrap_or(0.0)
            })
            .filter(|h| *h > 0.0)
            .collect::<Vec<_>>(),
    );
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    let connected = reports.iter().filter(|r| r.connected).count();
    let joined = reports.iter().filter(|r| r.joined).count();
    let left = reports.iter().filter(|r| r.left).count();
    let snaps_total: u64 = reports.iter().map(|r| r.snapshots).sum();
    let mut snap_each: Vec<u64> = reports.iter().map(|r| r.snapshots).collect();
    snap_each.sort();
    let snap_p50 = if snap_each.is_empty() {
        0.0
    } else {
        snap_each[snap_each.len() / 2] as f64
    };
    let mut conns_ms: Vec<u128> = reports
        .iter()
        .filter(|r| r.connected)
        .map(|r| r.connect_ms)
        .collect();
    let in_bytes: u64 = reports.iter().map(|r| r.bytes_in).sum();
    let out_bytes: u64 = reports.iter().map(|r| r.bytes_out).sum();
    let moves: u64 = reports.iter().map(|r| r.moves).sum();
    let errors: u64 = reports.iter().map(|r| r.errors).sum();
    let join_rejected: u64 = reports.iter().map(|r| r.join_rejected).sum();
    let cap_rejected: u64 = reports.iter().map(|r| r.cap_rejected).sum();
    let budget_rejected: u64 = reports.iter().map(|r| r.budget_rejected).sum();
    let retrans_out: u64 = reports.iter().map(|r| r.retrans_out).sum();
    let dup_in: u64 = reports.iter().map(|r| r.dup_in).sum();
    let oob_dropped: u64 = reports.iter().map(|r| r.oob_dropped).sum();
    let gave_up: u64 = reports.iter().map(|r| r.gave_up).sum();
    let acks: u64 = reports.iter().map(|r| r.acks).sum();
    let ack_processed_max: u64 = reports
        .iter()
        .map(|r| r.ack_processed_max)
        .max()
        .unwrap_or(0);
    let ack_lag_max_ms: u128 = reports.iter().map(|r| r.ack_lag_max_ms).max().unwrap_or(0);
    let fulls: u64 = reports.iter().map(|r| r.fulls).sum();
    let private_fulls: u64 = reports.iter().map(|r| r.private_fulls).sum();
    let deltas: u64 = reports.iter().map(|r| r.deltas).sum();
    let gap_drops: u64 = reports.iter().map(|r| r.gap_drops).sum();
    let view_size_total: u64 = reports.iter().map(|r| r.view_size).sum();
    let churn_cycles_total: u64 = reports.iter().map(|r| r.churn_cycles).sum();
    let resumed_total: u64 = reports.iter().map(|r| r.resumed).sum();
    let fresh_joins_total: u64 = reports.iter().map(|r| r.fresh_joins).sum();
    let hz_med = median(hzs);
    let dur = args.duration.as_secs_f64().max(1e-9);
    // Sessions the SERVER ended on its own (write stall, idle window,
    // violation budget, …). Cumulative and monotonic, so the report with
    // the largest total is the latest word on every reason — including a
    // post-teardown final report, which can only add. This is the number
    // `errors` cannot show: a session killed for not draining its socket
    // gets no ERROR frame through that socket, so its client sees only
    // silence, and a capacity run can shed half its clients with
    // `errors=0`.
    let server_closes = server_reports
        .iter()
        .map(|r| r.net.server_closes)
        .max_by_key(|c| c.total())
        .unwrap_or_default();
    let server_closes_total = server_closes.total();

    println!("=== gsb loadgen raw report ===");
    println!(
        "machine: cores={cores} profile={profile} mode={} clients_span={}..{}",
        mode_human,
        args.offset,
        args.offset + args.clients.saturating_sub(1)
    );
    println!(
        "clients: transport={} connected={connected}/{} joined={joined} left={left} errors={errors} server_closes={server_closes_total} join_rejected={join_rejected} cap_rejected={cap_rejected} budget_rejected={budget_rejected}",
        args.transport, args.clients
    );
    // `errors` counts what the CLIENTS saw; the server's own verdicts sit
    // next to it, and a non-zero total is called out so a summary reading
    // "errors=0" cannot pass for a clean run.
    println!(
        "server closes: total={server_closes_total} by_reason={}",
        server_closes.nonzero_summary()
    );
    if server_closes_total > 0 {
        println!(
            "WARNING: the server ended {server_closes_total} session(s) on its own initiative ({}); \
             errors={errors} counts only what the clients observed — this run is NOT clean",
            server_closes.nonzero_summary()
        );
    }
    // The rUDP client-side reliability picture (all zero on TCP): what
    // the clients' own reliable band had to do to keep the control path
    // loss-free (retrans_out = their retransmits; dup_in = the SERVER's
    // retransmits observed; gave_up = a control frame that never landed).
    if args.transport == gsb_server::TransportKind::Udp {
        println!(
            "udp client-side: retrans_out={retrans_out} dup_in={dup_in} oob_dropped={oob_dropped} gave_up={gave_up}",
        );
    }
    let slowest = reports
        .iter()
        .filter(|r| r.connected)
        .max_by_key(|r| r.connect_ms);
    println!(
        "connect{}: p50={}ms p99={}ms slowest={}ms (client #{})",
        if args.transport == gsb_server::TransportKind::Udp {
            " (handshake)"
        } else {
            ""
        },
        pctl(&mut conns_ms, 0.50),
        pctl(&mut conns_ms, 0.99),
        slowest.map(|r| r.connect_ms).unwrap_or(0),
        slowest.map(|r| r.id).unwrap_or(0)
    );
    println!(
        "snapshots: total={snaps_total} per_client_p50={snap_p50:.1} (window {}s)",
        args.duration.as_secs()
    );
    println!(
        "measured tick rate (snapshot sequence): median={hz_med:.2} Hz (configured 30.0 Hz; {} clients with >0.5s of snapshots)",
        hzs.len()
    );
    println!(
        "client bytes: in={} KB ({} KB/s) out={} KB moves={moves}",
        in_bytes / 1024,
        in_bytes as f64 / 1024.0 / dur,
        out_bytes / 1024
    );
    // The delta protocol + input-ack picture (client side): how the
    // snapshot stream split into fulls (fresh-group packets, keep-alive
    // fulls, one-shot private fulls) and deltas, how many deltas were
    // dropped as loss (healed by the next full), the final views, and the
    // ack stream (count, highest processed mark, worst lag).
    println!(
        "client view: fulls={} private_fulls={} deltas={} gap_drops={} final_view_total={} | acks={} ack_processed_max={} ack_lag_max_ms={}",
        fulls,
        private_fulls,
        deltas,
        gap_drops,
        view_size_total,
        acks,
        ack_processed_max,
        ack_lag_max_ms
    );

    let room = last_room_agg;
    let net = last_room.map(|l| &l.net);
    // Server-side bytes-out per connection (the AOI signal: AOI lowers this).
    let out_bps_per_conn = net
        .map(|n| (n.bytes_out_total as f64 / dur) / connected.max(1) as f64)
        .unwrap_or(0.0);
    if let Some(r) = room {
        // `hz` here is the run's *median* measured rate (the same value the
        // RESULT line reports as server_hz), NOT the final report's
        // window rate: the final report can be a 0-sample window (the
        // shutdown emit lands before the room's next 1 Hz sample, so its
        // Δsteps is 0 and its window rate reads 0.00) — printing that
        // would contradict the RESULT line in exactly the shutdown case
        // where a reader needs the two to agree.
        // Fine-histogram percentiles (sub-budget resolution; the log2
        // `step_p50_us~` above stays as the overflow-semantics view).
        // `FINE_HIST_CAP_US` marks "the rank is at/above the cap" —
        // unambiguous, since no fine-bin lower edge equals the cap
        // (they top out at 4088).
        // Against the folded HISTOGRAM's population, not `steps`: the two
        // histograms fold with SUM and `steps` with MAX, so on a sharded
        // room `steps` is a shard-count fraction of the distribution the
        // percentile is taken over (see `folded_steps`).
        let (p50_fine, p90_fine) = fine_percentiles_us(&r);
        println!(
            "server room (final): steps={} hz={:.2} budget_us={} step_min_us={} step_mean_us={:.1} step_max_us={} step_p50_us~{:.0} step_p99_us~{:.0} step_p50_fine_us={} step_p90_fine_us={} over_budget={:.1}% hist=[{}]",
            r.steps,
            server_hz,
            r.budget_us,
            r.step_min_us,
            r.step_mean_us,
            r.step_max_us,
            hist_percentile(&r.step_hist, r.budget_us, r.step_max_us, 0.50),
            hist_percentile(&r.step_hist, r.budget_us, r.step_max_us, 0.99),
            p50_fine,
            p90_fine,
            over_budget_frac(&r.step_hist) * 100.0,
            r.step_hist
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        println!(
            "server room (final): late_max_us={} lagged_events={} lagged_ticks={} dropped={} keepalive_resends={} snapshots={} max_payload_b={} snap_overflows={} out_bps_per_conn={:.0} shipped_frames={} private_frames={} mean_frame_b={:.0} groups={} members={} max_group={} joins={} leaves={} metrics_dropped={}",
            r.late_max_us,
            r.lagged_events,
            r.lagged_ticks,
            r.dropped,
            r.keepalive_resends,
            r.snapshots,
            r.snap_bytes_max,
            r.snap_overflows,
            out_bps_per_conn,
            r.shipped_frames,
            r.private_frames,
            // Mean shipped frame size: the datagram-transport half of
            // the outbound load (rUDP is bounded by packets as well as
            // by bytes, so the byte rate alone does not say it).
            if r.shipped_frames > 0 {
                r.shipped_bytes as f64 / r.shipped_frames as f64
            } else {
                0.0
            },
            r.groups,
            r.members,
            r.max_group,
            r.joins,
            r.leaves,
            r.metrics_dropped
        );
        println!(
            "overlap (steady state): records_per_tick={:.1} overlap_x={:.2} (peak members {})",
            rec_per_tick, overlap, peak_members
        );
        if let Some(g) = &last_room.and_then(|l| l.registry) {
            println!(
                "server registry (final): rooms={} conns={} opens={} closes={} joins={} leaves={} peak_conns={}",
                g.rooms, g.conns, g.opens, g.closes, g.joins, g.leaves, peak_conns
            );
        }
        if let Some(n) = net {
            println!(
                "server net (final): bytes_in={} KB ({} KB/s) bytes_out={} KB ({} KB/s) frames_in={} frames_out={} actions_dropped={}",
                n.bytes_in / 1024,
                n.bytes_in as f64 / 1024.0 / dur,
                n.bytes_out_total / 1024,
                n.bytes_out_total as f64 / 1024.0 / dur,
                n.frames_in,
                n.frames_out,
                n.actions_dropped
            );
            if !last_room.as_ref().unwrap().actions_dropped_top.is_empty() {
                // Per-connection attribution (the fairness guardrail's
                // receipt: drops live on the flooder's own channel, not on
                // anyone else's input). Bounded (≤ 5 entries), so the
                // clone is free.
                let top = last_room.as_ref().unwrap().actions_dropped_top.clone();
                println!(
                    "server net (final): actions_dropped_top={}",
                    top.iter()
                        .map(|(c, n)| format!("c{}:{}", c.0, n))
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
        }
    } else {
        println!("server metrics: unavailable (external mode)");
    }

    // Machine-parseable summary (consumed by tests/loadgen_smoke.rs).
    println!(
        "RESULT mode={} visibility={} shards={} max_snap_bytes={} clients={} connected={} joined={} left={} snap_total={} \
         snap_per_client_p50={:.1} tick_hz_med={:.2} client_in_bps={} client_out_bps={} \
         out_bps_per_conn={:.0} moves={} errors={} server_closes={} steps={} server_hz={:.2} \
         step_p50_us={:.0} step_p50_fine_us={} step_p90_fine_us={} step_max_us={} step_over_budget_pct={:.1} dropped={} late_max_us={} \
         peak_payload_b={} snap_overflows={} records_per_tick={:.1} overlap_x={:.2} \
         server_in_bps={} server_out_bps={} peak_conns={} metrics_dropped={} \
         profile={} offset={} procs={} server_pid={} client_pids={} affinity={} \
         server_cpu_s={:.1} clients_cpu_s={:.1} \
          join_rejected={} cap_rejected={} budget_rejected={} actions_dropped={} \
           actions_dropped_top={} transport={} retrans_out={} dup_in={} oob_dropped={} \
            gave_up={} acks={} ack_processed_max={} ack_lag_max_ms={} fulls={} \
            private_fulls={} deltas={} gap_drops={} view_size={} still_frac={} \
            req_local={} req_ext={} req_rej_malformed={} req_rej_dup={} \
            req_rej_no_handler={} req_rej_logic={} req_rej_conn={} req_rej_room={} \
            req_to={} req_late={} req_pending={} churn_cycles={} resumed={} \
             fresh_joins={} room_resumes={} resume_rejected_stale={} \
             detach_expired_ai={} detach_expired_despawn={}{} game={}",
        mode,
        args.visibility,
        // Shard-aware like the legacy spelling: the EXPLICIT topology key
        // decides when present (an operator running
        // `--topology sharded --visibility spatial` IS on the grid even
        // though the legacy spelling says spatial); without it the legacy
        // derivation applies.
        match args.topology {
            Some(gsb_server::Topology::Sharded) => args.shard_count,
            Some(gsb_server::Topology::Single) => 1,
            None if args.visibility == gsb_server::Visibility::Sharded => args.shard_count,
            None => 1,
        },
        args.max_snapshot_bytes,
        args.clients,
        connected,
        joined,
        left,
        snaps_total,
        snap_p50,
        hz_med,
        (in_bytes as f64 / dur) as u64,
        (out_bytes as f64 / dur) as u64,
        out_bps_per_conn,
        moves,
        errors,
        server_closes_total,
        room.map(|r| r.steps).unwrap_or(0),
        server_hz,
        room.map(|r| hist_percentile(&r.step_hist, r.budget_us, r.step_max_us, 0.50))
            .unwrap_or(0.0),
        room.map(|r| fine_percentiles_us(&r).0)
            .unwrap_or(FINE_HIST_CAP_US),
        room.map(|r| fine_percentiles_us(&r).1)
            .unwrap_or(FINE_HIST_CAP_US),
        room.map(|r| r.step_max_us).unwrap_or(0),
        room.map(|r| over_budget_frac(&r.step_hist) * 100.0)
            .unwrap_or(0.0),
        room.map(|r| r.dropped).unwrap_or(0),
        room.map(|r| r.late_max_us).unwrap_or(0),
        room.map(|r| r.snap_bytes_max as u64).unwrap_or(0),
        room.map(|r| r.snap_overflows).unwrap_or(0),
        rec_per_tick,
        overlap,
        net.map(|n| (n.bytes_in as f64 / dur) as u64).unwrap_or(0),
        net.map(|n| (n.bytes_out_total as f64 / dur) as u64)
            .unwrap_or(0),
        peak_conns,
        last_room.map(|l| l.metrics_dropped).unwrap_or(0),
        match args.profile {
            Profile::Ring => "ring",
            Profile::Spread => "spread",
            Profile::Still => "still",
        },
        args.offset,
        sep.map(|s| s.procs).unwrap_or(1),
        sep.map(|s| s.server_pid).unwrap_or(0),
        sep.map(|s| s
            .client_pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(","))
            .unwrap_or_else(|| "0".to_string()),
        sep.map(|s| s.affinity.clone())
            .unwrap_or_else(|| "none".to_string()),
        sep.map(|s| s.server_cpu_s).unwrap_or(0.0),
        sep.map(|s| s.clients_cpu_s).unwrap_or(0.0),
        join_rejected,
        cap_rejected,
        budget_rejected,
        net.map(|n| n.actions_dropped).unwrap_or(0),
        last_room
            .map(|l| {
                l.actions_dropped_top
                    .iter()
                    .map(|(c, n)| format!("c{}:{}", c.0, n))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default(),
        args.transport,
        retrans_out,
        dup_in,
        oob_dropped,
        gave_up,
        acks,
        ack_processed_max,
        ack_lag_max_ms,
        fulls,
        private_fulls,
        deltas,
        gap_drops,
        view_size_total,
        args.still_frac,
        // The room's RPC counters (zero on the smoke scenarios: they
        // carry no RPC traffic — their presence in the line and their
        // zero values are what the smoke asserts on; a shifted metric
        // queue would surface here as missing keys or garbage values).
        room.map(|r| r.requests_local).unwrap_or(0),
        room.map(|r| r.requests_external).unwrap_or(0),
        room.map(|r| r.requests_rejected_malformed).unwrap_or(0),
        room.map(|r| r.requests_rejected_dup).unwrap_or(0),
        room.map(|r| r.requests_rejected_no_handler).unwrap_or(0),
        room.map(|r| r.requests_rejected_logic).unwrap_or(0),
        room.map(|r| r.requests_rejected_conn_cap).unwrap_or(0),
        room.map(|r| r.requests_rejected_room_cap).unwrap_or(0),
        room.map(|r| r.requests_timed_out).unwrap_or(0),
        room.map(|r| r.requests_late).unwrap_or(0),
        room.map(|r| r.pending_requests).unwrap_or(0),
        // The churn profile's numbers (RECONNECT §14.5): client-side cycle
        // counts, and the server-side cumulative resume counters from the
        // room report (zero on a plain run — their presence is the queue
        // check).
        churn_cycles_total,
        resumed_total,
        fresh_joins_total,
        room.map(|r| r.resumes).unwrap_or(0),
        room.map(|r| r.resume_rejected_stale).unwrap_or(0),
        room.map(|r| r.detach_expired_ai).unwrap_or(0),
        room.map(|r| r.detach_expired_despawn).unwrap_or(0),
        // One key per server-close reason (`server_close_<reason>=N`,
        // zeros included — every key always present, like the req_rej_*
        // family). `server_closes=` above is the single total to grep.
        server_closes
            .iter()
            .map(|(r, n)| format!(" server_close_{}={n}", r.label()))
            .collect::<String>(),
        // The hosted game, the line's LAST key (GAME-MODULE §4.5: the one
        // addition; every key before it keeps its place and format).
        args.game,
    );
}
