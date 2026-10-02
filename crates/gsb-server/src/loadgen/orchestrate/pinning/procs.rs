//! Spawning and watching the pinned child processes.

use super::*;
use gsb_core::metrics::MetricReport;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc;

mod child_args;
use child_args::*;
mod server_child;
use server_child::*;

/// Spawn a child process, optionally pinned to `mask` (logical CPUs) via
/// `taskset -c`. Stdout is piped only when `pipe_stdout` (the client
/// processes' CLIENT lines, the server's `SERVING` line); stderr is
/// always inherited (visible).
pub(crate) async fn spawn_pinned(
    exe: &std::path::Path,
    args: &[String],
    env: &[(String, String)],
    mask: &Option<Vec<u32>>,
    taskset: Option<&std::path::Path>,
    label: &str,
    pipe_stdout: bool,
) -> std::io::Result<Child> {
    let mut cmd = match (mask, taskset) {
        (Some(mask), Some(ts)) => {
            let cpus = mask
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            eprintln!("orchestrate: {label} pinned to cores {cpus}");
            let mut c = Command::new(ts);
            c.arg("-c").arg(cpus).arg(exe);
            c
        }
        _ => Command::new(exe),
    };
    for a in args {
        cmd.arg(a);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdout(if pipe_stdout {
        Stdio::piped()
    } else {
        Stdio::inherit()
    });
    cmd.stderr(Stdio::inherit());
    cmd.spawn()
}

/// Sum of utime+stime (clock ticks) from `/proc/<pid>/stat`. The comm
/// field may contain spaces/parens, so cut at the LAST ')': utime and
/// stime are then fields 12 and 13 of the remainder (1-based 14/15).
pub(crate) fn proc_ticks(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = s.rsplit(')').next()?.split_whitespace();
    let mut it = rest;
    let utime: u64 = it.nth(11)?.parse().ok()?; // field 14 (1-based)
    let stime: u64 = it.next()?.parse().ok()?; // field 15 (1-based)
    Some(utime + stime)
}

/// One child's stdout, line by line, into a channel (the reader task's
/// only awaited source: the pipe).
pub(crate) async fn read_lines(stdout: ChildStdout, tx: mpsc::UnboundedSender<String>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                if tx.send(line.trim().to_string()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// A client record merged from a child's `CLIENT` line (item A): the
/// per-client raw values the orchestrator needs to recompute the report
/// exactly (sums, percentiles over raw values, hz median).
pub(crate) struct ClientRec {
    pub(crate) id: u64,
    pub(crate) connected: bool,
    pub(crate) connect_ms: u128,
    pub(crate) joined: bool,
    pub(crate) left: bool,
    pub(crate) snapshots: u64,
    pub(crate) bytes_in: u64,
    pub(crate) bytes_out: u64,
    pub(crate) moves: u64,
    pub(crate) errors: ClientErrors,
    pub(crate) join_rejected: u64,
    pub(crate) cap_rejected: u64,
    pub(crate) budget_rejected: u64,
    pub(crate) retrans_out: u64,
    pub(crate) dup_in: u64,
    pub(crate) oob_dropped: u64,
    pub(crate) gave_up: u64,
    pub(crate) frag_reassembled: u64,
    pub(crate) frag_dropped: u64,
    pub(crate) hs_retries: u64,
    pub(crate) acks: u64,
    pub(crate) ack_processed_max: u64,
    pub(crate) ack_lag_max_ms: u128,
    pub(crate) fulls: u64,
    pub(crate) private_fulls: u64,
    pub(crate) deltas: u64,
    pub(crate) gap_drops: u64,
    pub(crate) view_size: u64,
    pub(crate) churn_cycles: u64,
    pub(crate) resumed: u64,
    pub(crate) fresh_joins: u64,
    pub(crate) hz: Option<f64>,
}

pub(crate) fn parse_client_line(line: &str) -> Option<ClientRec> {
    let rest = line.strip_prefix("CLIENT ")?;
    let get = |k: &str| -> Option<String> {
        rest.split_whitespace()
            .find_map(|p| p.strip_prefix(&format!("{k}=")).map(str::to_string))
    };
    Some(ClientRec {
        id: get("id")?.parse().ok()?,
        connected: get("connected")?.as_str() == "true",
        connect_ms: get("connect_ms")?.parse().ok()?,
        joined: get("joined")?.as_str() == "true",
        left: get("left")?.as_str() == "true",
        snapshots: get("snapshots")?.parse().ok()?,
        bytes_in: get("bytes_in")?.parse().ok()?,
        bytes_out: get("bytes_out")?.parse().ok()?,
        moves: get("moves")?.parse().ok()?,
        errors: {
            // Every reason (B88); `errors=` is their sum, and must be.
            let mut v = [0u64; ERROR_REASONS];
            for (slot, (k, _)) in v.iter_mut().zip(ClientErrors::default().fields()) {
                *slot = get(k)?.parse().ok()?;
            }
            let e = ClientErrors::from_values(v);
            (get("errors")?.parse::<u64>().ok()? == e.total()).then_some(e)?
        },
        join_rejected: get("join_rejected")?.parse().ok()?,
        cap_rejected: get("cap_rejected")?.parse().ok()?,
        budget_rejected: get("budget_rejected")?.parse().ok()?,
        retrans_out: get("retrans_out")?.parse().ok()?,
        dup_in: get("dup_in")?.parse().ok()?,
        oob_dropped: get("oob_dropped")?.parse().ok()?,
        gave_up: get("gave_up")?.parse().ok()?,
        frag_reassembled: get("frag_reassembled")?.parse().ok()?,
        frag_dropped: get("frag_dropped")?.parse().ok()?,
        hs_retries: get("hs_retries")?.parse().ok()?,
        acks: get("acks")?.parse().ok()?,
        ack_processed_max: get("ack_processed_max")?.parse().ok()?,
        ack_lag_max_ms: get("ack_lag_max_ms")?.parse().ok()?,
        fulls: get("fulls")?.parse().ok()?,
        private_fulls: get("private_fulls")?.parse().ok()?,
        deltas: get("deltas")?.parse().ok()?,
        gap_drops: get("gap_drops")?.parse().ok()?,
        view_size: get("view_size")?.parse().ok()?,
        churn_cycles: get("churn_cycles").unwrap_or_default().parse().ok()?,
        resumed: get("resumed").unwrap_or_default().parse().ok()?,
        fresh_joins: get("fresh_joins").unwrap_or_default().parse().ok()?,
        hz: match get("hz")?.as_str() {
            "-" => None,
            v => v.parse().ok(),
        },
    })
}

/// How long the metric reader may still run once the server child has
/// exited (see its await in [`orchestrate()`]).
const METRICS_READER_GRACE: Duration = Duration::from_secs(5);

/// The orchestrator (item A). Topology (all on loopback, all spawned by
/// this process):
///
/// ```text
/// orchestrator (this process)
///   ├─ server process   gsb-loadgen --serve --bind 127.0.0.1:0
///   │                   --metrics-listen 127.0.0.1:0
///   │      (binds both, prints `SERVING addr=127.0.0.1:P
///   │      metrics=127.0.0.1:M` on stdout — the only thing read from
///   │      it; its MetricReports arrive over the binary socket — the
///   │      same channel data the in-process mode drains)
///   ├─ client process 0 gsb-loadgen N0 --addr 127.0.0.1:P --offset 0
///   ├─ client process 1 gsb-loadgen N1 --addr 127.0.0.1:P --offset N0
///   └─ client process P-1
/// ```
///
/// What this buys / gives up (the item-A design decision, required by
/// the spec):
/// - **Buys:** the server runs in its own process with its own runtime;
///   with `--pin` it is pinned (taskset) to a disjoint set of physical
///   cores, so its CPU is *measurably* isolated from the clients' decode
///   work — the wall that capped the in-process D1 run (~6 GB/s of
///   client-side protobuf decode sharing cores with the room actor at
///   10k). The per-process CPU seconds in RESULT prove it a posteriori.
/// - **Gives up (1):** the per-client arrival `Instant`s no longer exist
///   in this process; the children print them (precomputed hz) and the
///   raw connect_ms on `CLIENT` lines, which this process merges. Sums
///   and percentiles are computed over the merged *raw* values, so the
///   merged report is exact — not an aggregation of aggregates.
/// - **Gives up (2):** the orchestrator must be the parent of both sides
///   (it spawns the server too), so "external server + these clients"
///   remains the separate `--addr` mode for servers this tool does not
///   run.
pub(crate) async fn orchestrate(args: Args) {
    init_tracing();
    let n = args.clients;
    let mut procs = args.procs.max(1);
    if (procs as u64) > n {
        procs = n as u32;
    }

    // Affinity (optional): disjoint core sets from the real topology.
    let taskset = which_taskset();
    let masks = if args.pin {
        pin_masks(args.pin_server_cores, procs)
    } else {
        None
    };
    if args.pin && (masks.is_none() || taskset.is_none()) {
        eprintln!(
            "orchestrate: --pin ineffective (taskset={}, topology readable); running UNPINNED — the CPU isolation then rests on process+runtime separation alone",
            taskset.is_some()
        );
    }

    eprintln!(
        "orchestrate: game={} N={n} procs={procs} visibility={} profile={} cell_size={} vision_radius={} max_snap_bytes={} spawn_half={} duration={}s move_ms={} stagger_ms={} pin={}",
        args.game,
        args.visibility,
        match args.profile {
            Profile::Ring => "ring",
            Profile::Spread => "spread",
            Profile::Still => "still",
        },
        if args.visibility == gsb_server::Visibility::Spatial {
            args.cell_size.to_string()
        } else {
            "-".into()
        },
        if args.visibility == gsb_server::Visibility::Team {
            args.vision_radius.to_string()
        } else {
            "-".into()
        },
        args.max_snapshot_bytes,
        args.server_spawn_half,
        args.duration.as_secs(),
        args.move_ms.as_millis(),
        args.stagger_ms,
        masks.is_some()
    );

    let exe = std::env::current_exe().expect("current exe");

    // ── server child ──────────────────────────────────────────────────
    // The server's workers are sized to its pinned core set; unpinned it
    // gets the operator's `--workers` or its runtime default (B37,
    // `child_workers`). Its command line (`server_args`) runs it 3 s past
    // the clients, on port 0 for both of its doors: it reports the ports
    // it bound, and a child that dies or stays silent first ends the run
    // here, before any client child exists (BACKLOG F31).
    let server_cores = masks.as_ref().map(|m| m.0.len().max(1));
    let sargs = server_args(&args, server_cores);
    let server_mask = masks.as_ref().map(|m| m.0.clone());
    let ServerChild {
        child: mut server,
        pid: server_pid,
        addr: server_addr,
        metrics: metrics_addr,
        stdout_forward,
    } = match start_server_child(
        &exe,
        &sargs,
        &server_mask,
        taskset.as_deref(),
        SERVER_REPORT_BOUND,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("orchestrate: {e}; no run");
            std::process::exit(1);
        }
    };
    eprintln!("orchestrate: server child serves {server_addr} (metrics {metrics_addr})");
    probe_door(args.transport, server_addr).await;

    // ── client children ───────────────────────────────────────────────
    // Partition N clients across `procs` children (first `N % procs`
    // children get +1); offsets keep the global ids contiguous, so the
    // stagger and the profile's per-id determinism span the whole run.
    let base = n / procs as u64;
    let rem = n % procs as u64;
    let mut children = Vec::with_capacity(procs as usize);
    let mut client_pids = Vec::with_capacity(procs as usize);
    let mut line_txs = Vec::new();
    let mut offset = 0u64;
    for p in 0..procs as u64 {
        let count = base + (if p < rem { 1 } else { 0 });
        let cores = masks
            .as_ref()
            .and_then(|m| m.1.get(p as usize))
            .map(|m| m.len().max(1));
        let cargs = client_args(&args, count, offset, server_addr, cores);
        // The client process prints its per-client records (env-gated).
        let env = [("GSB_LOADGEN_CLIENT_LINES".to_string(), "1".to_string())];
        let mut child = spawn_pinned(
            &exe,
            &cargs,
            &env,
            &masks.as_ref().and_then(|m| m.1.get(p as usize).cloned()),
            taskset.as_deref(),
            &format!("client{p}"),
            true,
        )
        .await
        .unwrap_or_else(|e| panic!("spawn client{p} child: {e}"));
        let stdout = child.stdout.take().expect("stdout piped");
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let reader_tx = tx.clone();
        line_txs.push((tx, tokio::spawn(read_lines(stdout, reader_tx)), rx));
        client_pids.push(child.id().expect("freshly spawned child has a pid"));
        children.push(child);
        offset += count;
    }
    // Baseline CPU ticks (USER_HZ = 100 on Linux): isolation proof
    // starts before any load.
    let pids = std::iter::once(server_pid)
        .chain(client_pids.iter().copied())
        .collect::<Vec<u32>>();
    let t0: Vec<Option<u64>> = pids.iter().map(|&p| proc_ticks(p)).collect();

    // ── metric reports from the server socket ─────────────────────────
    let mut metrics_task = tokio::spawn(async move {
        // The listener was bound before the child reported it, so one
        // connect is enough: a refusal means the child is already gone.
        let mut stream = match TcpStream::connect(metrics_addr).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("orchestrate: metrics connect {metrics_addr}: {e}");
                return Vec::new();
            }
        };
        let mut all = Vec::new();
        let mut header = [0u8; 8];
        loop {
            if stream.read_exact(&mut header).await.is_err() {
                break;
            }
            let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
            if magic != METRICS_MAGIC {
                eprintln!("orchestrate: bad metrics magic {magic:#x}");
                break;
            }
            let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            if len > 1024 * 1024 {
                eprintln!("orchestrate: implausible report size {len}");
                break;
            }
            let mut body = vec![0u8; len];
            if stream.read_exact(&mut body).await.is_err() {
                break;
            }
            match decode_report(&body) {
                Some(r) => all.push(r),
                None => {
                    eprintln!("orchestrate: undecodable metric report");
                    break;
                }
            }
        }
        all
    });

    // ── wait for the client children ──────────────────────────────────
    // Round-robin poll with a 250 ms budget per round. CPU accounting
    // rides the same polls: /proc/<pid>/stat vanishes at reap, so t1
    // must be sampled while each child is still alive — the LAST
    // successful sample per child (every round, until it exits) is the
    // t1 used below. Repeated `wait()` calls are safe (tokio keeps the
    // shared child; a finished child returns immediately).
    let mut t1_clients: Vec<Option<u64>> = vec![None; pids.len()];
    let child_margin = Instant::now() + args.duration + Duration::from_secs(30);
    let mut pending: Vec<usize> = (0..children.len()).collect();
    let mut client_exit_ok = true;
    while !pending.is_empty() && Instant::now() < child_margin {
        let mut next = Vec::new();
        for &ci in &pending {
            if let Some(t) = proc_ticks(client_pids[ci]) {
                t1_clients[ci + 1] = Some(t); // pids[0] is the server
            }
            match tokio::time::timeout(Duration::from_millis(250), children[ci].wait()).await {
                Ok(res) => {
                    client_exit_ok &= res.map(|s| s.success()).unwrap_or(false);
                }
                Err(_) => next.push(ci),
            }
        }
        pending = next;
    }
    for &ci in &pending {
        eprintln!("orchestrate: client child {ci} timed out; killing");
        let _ = children[ci].kill().await;
        let _ = children[ci].wait().await;
        client_exit_ok = false;
    }
    if !client_exit_ok {
        eprintln!(
            "orchestrate: WARNING — a client child exited non-success; the merged numbers below cover what it reported"
        );
    }

    // Drain each child's lines (the reader tasks exit on pipe EOF, which
    // follows child exit; dropping the extra senders unblocks them).
    let mut recs: Vec<ClientRec> = Vec::new();
    for (tx, _reader, rx) in line_txs {
        drop(tx);
        let mut rx = rx;
        while let Some(line) = rx.recv().await {
            if let Some(r) = parse_client_line(&line) {
                recs.push(r);
            } else if line.starts_with("RESULT ") {
                eprintln!("orchestrate: child result: {line}");
            }
        }
    }
    recs.sort_by_key(|r| r.id);
    if (recs.len() as u64) != n {
        eprintln!(
            "orchestrate: WARNING — merged {} client records, expected {n} (a child died before reporting?)",
            recs.len()
        );
    }

    // ── wait for the server child (clean stop at duration + 3 s) ──────
    // Same CPU sampling as the clients: last successful /proc read while
    // the server is alive is t1_server (every poll round until exit).
    let mut t1_server: Vec<Option<u64>> = vec![None; pids.len()];
    let server_margin = Instant::now() + args.duration + Duration::from_secs(30);
    let server_exit;
    loop {
        if let Some(t) = proc_ticks(server_pid) {
            t1_server[0] = Some(t);
        }
        match tokio::time::timeout(Duration::from_millis(250), server.wait()).await {
            Ok(res) => {
                server_exit = Some(res);
                break;
            }
            Err(_) if Instant::now() >= server_margin => {
                eprintln!("orchestrate: server child timed out; killing");
                let _ = server.kill().await;
                server_exit = Some(server.wait().await);
                break;
            }
            Err(_) => {}
        }
    }
    if let Some(res) = server_exit
        && !res.map(|s| s.success()).unwrap_or(false)
    {
        eprintln!("orchestrate: WARNING — server child exited non-success");
    }
    // The metric stream ends when the server's export task ends (its
    // channel closes on the clean stop), so the reader is done moments
    // after the child's exit; the grace only bounds a stream that, for
    // whatever reason, outlives the child (F25: a reader that could never
    // connect used to retry forever — it now connects once, to a listener
    // the child bound before reporting it, F31).
    // The child's last stdout lines go out before the report.
    let _ = tokio::time::timeout(METRICS_READER_GRACE, stdout_forward).await;
    let server_reports: Vec<MetricReport> = match tokio::time::timeout(
        METRICS_READER_GRACE,
        &mut metrics_task,
    )
    .await
    {
        Ok(reports) => reports.expect("metrics reader panicked"),
        Err(_) => {
            metrics_task.abort();
            eprintln!(
                "orchestrate: WARNING — the server child served no metric stream; server-side numbers are empty"
            );
            Vec::new()
        }
    };

    // CPU seconds per process over the run (ticks / USER_HZ). Each side
    // uses its own t1 (sampled while that side was still alive): the
    // server's t1_server, the clients' t1_clients.
    let delta = |i: usize, t1: &Vec<Option<u64>>| -> f64 {
        match (t0.get(i).copied().flatten(), t1.get(i).copied().flatten()) {
            (Some(a), Some(b)) if b >= a => (b - a) as f64 / 100.0,
            _ => 0.0,
        }
    };
    let server_cpu_s = delta(0, &t1_server);
    let clients_cpu_s: f64 = (1..pids.len()).map(|i| delta(i, &t1_clients)).sum();

    // Merge the per-client records into the report's input (the same
    // structures the in-process path builds; seq instants are
    // process-local and were consumed by the children into their hz).
    let reports: Vec<ClientReport> = recs
        .iter()
        .map(|c| ClientReport {
            id: c.id,
            connected: c.connected,
            connect_ms: c.connect_ms,
            joined: c.joined,
            entity: 0,
            left: c.left,
            snapshots: c.snapshots,
            bytes_in: c.bytes_in,
            bytes_out: c.bytes_out,
            moves: c.moves,
            errors: c.errors,
            join_rejected: c.join_rejected,
            cap_rejected: c.cap_rejected,
            budget_rejected: c.budget_rejected,
            retrans_out: c.retrans_out,
            dup_in: c.dup_in,
            oob_dropped: c.oob_dropped,
            gave_up: c.gave_up,
            frag_reassembled: c.frag_reassembled,
            frag_dropped: c.frag_dropped,
            hs_retries: c.hs_retries,
            acks: c.acks,
            ack_processed_max: c.ack_processed_max,
            ack_lag_max_ms: c.ack_lag_max_ms,
            fulls: c.fulls,
            private_fulls: c.private_fulls,
            deltas: c.deltas,
            gap_drops: c.gap_drops,
            view_size: c.view_size,
            churn_cycles: c.churn_cycles,
            resumed: c.resumed,
            fresh_joins: c.fresh_joins,
            // The orchestrated run refuses the RPC mode (`args/check.rs`).
            rpc: RpcTally::default(),
            seq_first: None,
            seq_last: None,
        })
        .collect();
    let hzs: Vec<f64> = recs.iter().filter_map(|c| c.hz).collect();

    // The affinity record (the RESULT line keeps it in one token).
    let affinity = match &masks {
        Some((server, clients)) => {
            let f = |v: &[u32]| v.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            let mut s = format!("server:{}", f(server));
            for (i, c) in clients.iter().enumerate() {
                s.push_str(&format!(";client{i}:{}", f(c)));
            }
            s
        }
        None => "none".to_string(),
    };
    let sep = SepInfo {
        procs,
        server_pid,
        client_pids,
        affinity,
        server_cpu_s,
        clients_cpu_s,
    };
    print_report(&args, false, &reports, &hzs, &server_reports, Some(&sep));
}
