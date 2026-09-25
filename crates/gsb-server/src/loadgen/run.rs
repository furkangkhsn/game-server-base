//! The run itself: spawn the clients, hold the window open, collect.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use gsb_core::metrics::MetricReport;

use super::*;
use crate::churn::*;
use crate::client::*;
use crate::report::*;
use crate::server::*;
use crate::stats::*;

pub(crate) async fn run(args: Args) {
    init_tracing();

    // The in-process handle stays with the main task: it must be stopped
    // only AFTER the clients are done, so it cannot be spawned early.
    let (addr, inproc, rep_rx, server) = match &args.addr {
        Some(addr) => {
            let a: SocketAddr = addr.parse().expect("valid --addr HOST:PORT");
            eprintln!(
                "mode: external server at {a} (transport={}; client-side numbers only)",
                args.transport
            );
            (a, false, None, None)
        }
        None => {
            let s = start_inprocess(
                args.game,
                args.visibility,
                args.topology,
                args.shard_count,
                args.cell_size,
                args.vision_radius,
                args.max_snapshot_bytes,
                args.server_spawn_half,
                args.transport,
                ServerOverrides {
                    max_players: args.max_players,
                    max_connections: args.max_connections,
                    idle_timeout_secs: args.idle_timeout_secs,
                    write_stall_secs: args.write_stall_secs,
                    disconnect_grace_secs: args.disconnect_grace_secs,
                    mmo_crystallize: args.mmo_crystallize,
                },
            )
            .await
            .expect("server starts");
            let addr = s.handle.addr;
            eprintln!(
                "mode: in-process server at {addr} (transport={}; clients share CPU with server)",
                args.transport
            );
            (addr, true, Some(s.rep_rx), Some(s.handle))
        }
    };

    eprintln!(
        "clients={} offset={} room={} duration={}s move_ms={} stagger_ms={} visibility={} cell_size={} vision_radius={} max_snap_bytes={} profile={}",
        args.clients,
        args.offset,
        args.room,
        args.duration.as_secs(),
        args.move_ms.as_millis(),
        args.stagger_ms,
        args.visibility,
        if args.visibility == gsb_server::Visibility::Spatial {
            args.cell_size.to_string()
        } else {
            "-".to_string()
        },
        if args.visibility == gsb_server::Visibility::Team {
            args.vision_radius.to_string()
        } else {
            "-".to_string()
        },
        args.max_snapshot_bytes,
        match args.profile {
            Profile::Ring => "ring".to_string(),
            Profile::Spread => "spread".to_string(),
            Profile::Still => format!("still(={:.2})", args.still_frac),
        }
    );

    let bot = crate::bot::bot_for(&args);
    eprintln!("bot: {}", bot.describe());

    // Spawn the report drain (one task, one awaited source), then the N
    // clients (ids `offset..offset+N` — the stagger and the profile's
    // per-id determinism use the global id, so a partitioned run over
    // several processes is identical to one in-process run). The main
    // task collects client results with plain sequential awaits — the
    // clients all run in parallel anyway.
    let drain = rep_rx.map(|rx| tokio::spawn(drain_reports(rx)));
    let deadline = Instant::now() + args.duration;
    let n = args.clients;
    let mut p = ClientParams {
        addr,
        tls: args.tls_ca.clone().map(|ca_path| TlsOpts {
            ca_path,
            server_name: args.tls_server_name.clone(),
        }),
        room: args.room,
        move_ms: args.move_ms,
        stagger_ms: args.stagger_ms,
        bot,
        deadline,
        flood: false,
        kind: args.transport,
        capture: None,
    };
    if let Some(dir) = &args.capture {
        std::fs::create_dir_all(dir)
            .unwrap_or_else(|e| panic!("--capture: cannot create `{dir}`: {e}"));
        eprintln!(
            "capture: {} of {n} clients' game-band frames into {dir}",
            args.capture_clients.min(n)
        );
    }
    // Churn mode swaps the client BODY per task (RECONNECT §14.5); the
    // plain path below stays byte-identical to every previous measurement.
    let churn_cycle = args.churn_secs.map(Duration::from_secs_f64);
    eprintln!(
        "profile_note: {}",
        match (&churn_cycle, args.disconnect_grace_secs) {
            (Some(c), g) => format!("CHURN cycle={c:?} grace={g:?} (keep grace > cycle ⇒ resumes)"),
            (None, _) => "plain".to_string(),
        }
    );
    let mut clients = Vec::with_capacity(n as usize);
    for i in 0..n {
        let id = args.offset + i;
        // The `--flood-id` client (by GLOBAL id) runs the tight-write flood
        // after joining; every other client is paced normally.
        p.flood = args.flood_id == Some(id);
        p.capture = args
            .capture
            .as_ref()
            .filter(|_| crate::capture::captured(i, n, args.capture_clients))
            .map(|dir| {
                let path = std::path::Path::new(dir).join(format!("client-{id}.gsbcap"));
                (path, args.game)
            });
        match churn_cycle {
            Some(cycle) => clients.push(tokio::spawn(run_churn_client(
                id,
                p.clone(),
                cycle,
                args.churn_cycles,
            ))),
            None => clients.push(tokio::spawn(run_client(id, p.clone()))),
        }
    }
    let mut reports = Vec::with_capacity(clients.len());
    for h in clients {
        reports.push(h.await.expect("client task panicked"));
    }
    // Small grace period: let the leave acks, the connection actors'
    // final metric flushes, and the registry's leave flushes settle.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Machine-readable per-client records for the orchestrator (item A):
    // it merges them into one final report. Gated by an env var — at load
    // scale these lines would otherwise bury the human report — and
    // harmless otherwise (direct mode ignores them).
    if std::env::var_os("GSB_LOADGEN_CLIENT_LINES").is_some() {
        for r in &reports {
            println!(
                "CLIENT id={} connected={} connect_ms={} joined={} left={} snapshots={} \
                  bytes_in={} bytes_out={} moves={} errors={} join_rejected={} cap_rejected={} \
                  budget_rejected={} retrans_out={} dup_in={} oob_dropped={} gave_up={} \
                  frag_reassembled={} frag_dropped={} hs_retries={} \
                  acks={} ack_processed_max={} ack_lag_max_ms={} fulls={} private_fulls={} \
                  deltas={} gap_drops={} view_size={} hz={} \
                  churn_cycles={} resumed={} fresh_joins={}",
                r.id,
                r.connected,
                r.connect_ms,
                r.joined,
                r.left,
                r.snapshots,
                r.bytes_in,
                r.bytes_out,
                r.moves,
                r.errors,
                r.join_rejected,
                r.cap_rejected,
                r.budget_rejected,
                r.retrans_out,
                r.dup_in,
                r.oob_dropped,
                r.gave_up,
                r.frag_reassembled,
                r.frag_dropped,
                r.hs_retries,
                r.acks,
                r.ack_processed_max,
                r.ack_lag_max_ms,
                r.fulls,
                r.private_fulls,
                r.deltas,
                r.gap_drops,
                r.view_size,
                match measured_hz(r) {
                    Some(h) => format!("{h:.3}"),
                    None => "-".to_string(),
                },
                r.churn_cycles,
                r.resumed,
                r.fresh_joins,
            );
        }
    }

    // Stop the server BEFORE awaiting the drain: the drain's channel
    // closes when the collector (its sender) exits, and the collector
    // exits when the ticker's broadcast closes — i.e. during stop().
    if let Some(handle) = server {
        handle.stop().await;
    }
    let server_reports: Vec<MetricReport> = match drain {
        Some(d) => d.await.expect("drain task panicked"),
        None => Vec::new(),
    };

    // The client-measured tick rates (the snapshot-sequence rate): here
    // the arrival instants are task-local, so they can be computed; the
    // orchestrator receives the same values pre-computed on the CLIENT
    // lines of its children.
    let hzs: Vec<f64> = reports.iter().filter_map(measured_hz).collect();

    print_report(&args, inproc, &reports, &hzs, &server_reports, None);
}
