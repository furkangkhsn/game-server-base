//! gsb load generator: N real-TCP clients against a real gsb server.
//!
//! Two modes:
//!
//! - **in-process** (default): the server is started inside this process
//!   (bound to an ephemeral port), the clients connect over real TCP, and
//!   the server's metric reports are captured in-process through
//!   [`gsb_server::start_server_metrics`] — a channel, no stdout parsing,
//!   no shared state. NOTE: the clients and the server then share the
//!   machine's cores (and the runtime), so the measured server capacity is
//!   a conservative **lower bound**; a dedicated load client would score
//!   higher.
//! - **external** (`--addr HOST:PORT`): connect to an externally running
//!   server (another machine or process); only client-side numbers are
//!   reported, and the server's own metrics come from that process's
//!   `gsb-metric` log lines.
//!
//! Usage:
//! ```text
//! gsb-loadgen [N] [--duration SECS] [--move-ms MS] [--room ID] [--stagger-ms MS] [--addr HOST:PORT]
//! ```
//! Defaults: N=100, duration=10 s, move interval 150 ms, room 1,
//! stagger 0 (all clients connect at once — the worst case for the accept
//! path over loopback; see `Args::stagger_ms`).
//!
//! Every client: connects, authenticates, joins the room, then until the
//! deadline sends a `MOVE_TO` around a circle (phase-shifted by client id
//! so the entities do not move in lockstep) and counts what it receives.
//! The entities' integer positions change on most ticks, so the room
//! re-emits its (single-group, full-world) snapshot nearly every tick —
//! the snapshot stream is the load the server has to fan out.
//!
//! The report ends with one machine-parseable `RESULT key=value ...` line
//! (consumed by the smoke test in `tests/loadgen_smoke.rs`).
//!
//! The client tasks follow the project's discipline: one task per client,
//! no `tokio::select!`, a bounded timeout on each read attempt; per-client
//! state is task-local and returned through the JoinHandle — nothing
//! shared.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use gsb_protocol::base::{
    Auth, Error, JoinRoom, JoinRoomResult, LeaveRoom, LeaveRoomResult,
};
use gsb_protocol::op;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, tcp::OwnedReadHalf};
use tokio::sync::mpsc;

use gsb_core::metrics::{hist_edge_us, MetricReport, HIST_OVERFLOW_BIN};

struct Args {
    clients: u64,
    duration: Duration,
    move_ms: Duration,
    room: u64,
    /// Client i connects after i × stagger_ms. 0 (default) = all at once.
    /// A burst of N simultaneous connects over loopback hits the tokio
    /// accept path in its worst case (edge-triggered wakeup per state
    /// change; see the load-test report); a stagger models the realistic
    /// trickle of players joining over time.
    stagger_ms: u64,
    addr: Option<String>,
    /// Enable AOI in the in-process server (spatial group key, `--aoi`).
    aoi: bool,
    /// AOI cell size in world units (`--cell-size N`, default 20).
    cell_size: f32,
    /// Per-snapshot payload ceiling the room enforces
    /// (`--max-snapshot-bytes N`, default 1400 = the rUDP MTU the spec
    /// assumes). The TCP transport default is 1 MiB, so the in-process
    /// server models the MTU-constrained scenario out of the box; `snap_*`
    /// overflow counters are only meaningful against this ceiling.
    max_snapshot_bytes: usize,
}

fn parse_args() -> Args {
    let mut args = Args {
        clients: 100,
        duration: Duration::from_secs(10),
        move_ms: Duration::from_millis(150),
        room: 1,
        stagger_ms: 0,
        addr: None,
        aoi: false,
        cell_size: 20.0,
        max_snapshot_bytes: 1400,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--duration" => {
                let v: f64 = it.next().expect("--duration SECS").parse().expect("number");
                args.duration = Duration::from_secs_f64(v);
            }
            "--move-ms" => {
                let v: u64 = it.next().expect("--move-ms MS").parse().expect("number");
                args.move_ms = Duration::from_millis(v);
            }
            "--room" => {
                args.room = it.next().expect("--room ID").parse().expect("number");
            }
            "--stagger-ms" => {
                let v: u64 = it.next().expect("--stagger-ms MS").parse().expect("number");
                args.stagger_ms = v;
            }
            "--addr" => {
                args.addr = Some(it.next().expect("--addr HOST:PORT"));
            }
            "--aoi" => {
                args.aoi = true;
            }
            "--cell-size" => {
                args.cell_size = it.next().expect("--cell-size N").parse().expect("number");
            }
            "--max-snapshot-bytes" => {
                args.max_snapshot_bytes =
                    it.next().expect("--max-snapshot-bytes N").parse().expect("number");
            }
            s if s.starts_with("--") => panic!("unknown flag {s}"),
            s => args.clients = s.parse().expect("N must be a number"),
        }
    }
    if args.clients == 0 {
        panic!("N must be > 0");
    }
    args
}

fn frame(op: u16, payload: &[u8]) -> Vec<u8> {
    let body = 2 + payload.len();
    let mut out = Vec::with_capacity(4 + body);
    out.extend_from_slice(&(body as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

async fn read_frame(r: &mut OwnedReadHalf) -> Option<(u16, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 4 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.ok()?;
    if body.len() < 2 {
        return None;
    }
    let op = u16::from_le_bytes([body[0], body[1]]);
    Some((op, body[2..].to_vec()))
}

/// One client's end-to-end record (task-local; returned via JoinHandle).
struct ClientReport {
    id: u64,
    connected: bool,
    connect_ms: u128,
    joined: bool,
    entity: u64,
    left: bool,
    snapshots: u64,
    bytes_in: u64,
    bytes_out: u64,
    moves: u64,
    errors: u64,
    /// First/last snapshot sequence with its arrival instant: the server's
    /// measured tick rate is (last_seq − first_seq) / Δt, since the
    /// snapshot sequence is the global tick index.
    seq_first: Option<(u64, Instant)>,
    seq_last: Option<(u64, Instant)>,
}

async fn run_client(
    id: u64,
    addr: SocketAddr,
    room: u64,
    move_ms: Duration,
    stagger_ms: u64,
    deadline: Instant,
) -> ClientReport {
    let mut rep = ClientReport {
        id,
        connected: false,
        connect_ms: 0,
        joined: false,
        entity: 0,
        left: false,
        snapshots: 0,
        bytes_in: 0,
        bytes_out: 0,
        moves: 0,
        errors: 0,
        seq_first: None,
        seq_last: None,
    };

    // Optional connect stagger (see `Args::stagger_ms`).
    if stagger_ms > 0 {
        tokio::time::sleep(Duration::from_millis(id * stagger_ms)).await;
    }
    let t0 = Instant::now();
    let stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("client {id}: connect failed: {e}");
            return rep;
        }
    };
    rep.connect_ms = t0.elapsed().as_millis();
    rep.connected = true;
    stream.set_nodelay(true).ok();
    let (mut r, mut w) = stream.into_split();

    // AUTH + JOIN in one write (the connection actor drains in order).
    let mut out = frame(op::base::AUTH_REQ, &Auth { name: format!("lg-{id}") }.encode_to_vec());
    out.extend(frame(op::base::JOIN_ROOM_REQ, &JoinRoom { room_id: room }.encode_to_vec()));
    rep.bytes_out += out.len() as u64;
    if w.write_all(&out).await.is_err() || w.flush().await.is_err() {
        return rep;
    }

    let t_start = Instant::now();
    let mut last_move = t_start;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now.duration_since(last_move) >= move_ms {
            last_move = now;
            // Circle of radius 40, phase-shifted per client (id-based
            // offset so N entities do not move in lockstep).
            let angle = (now.duration_since(t_start).as_secs_f64() + id as f64 * 0.618) * 4.0;
            let msg = gsb_game::game::MoveTo {
                x: (angle.cos() * 40.0) as i32,
                y: (angle.sin() * 40.0) as i32,
            };
            let f = frame(gsb_game::op::MOVE_TO, &msg.encode_to_vec());
            rep.moves += 1;
            rep.bytes_out += f.len() as u64;
            if w.write_all(&f).await.is_err() || w.flush().await.is_err() {
                break; // peer gone
            }
        }
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(250));
        let got = tokio::time::timeout(timeout, read_frame(&mut r))
            .await
            .ok()
            .flatten();
        let Some((op, payload)) = got else {
            continue; // timeout: loop
        };
        rep.bytes_in += (4 + 2 + payload.len()) as u64;
        match op {
            op::base::JOIN_ROOM_RESULT => {
                let m: JoinRoomResult = match JoinRoomResult::decode(&payload[..]) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                rep.joined = true;
                rep.entity = m.entity;
            }
            gsb_game::op::WORLD_SNAPSHOT => match gsb_game::game::WorldSnapshot::decode(&payload[..]) {
                Ok(m) => {
                    rep.snapshots += 1;
                    let at = Instant::now();
                    if rep.seq_first.is_none() {
                        rep.seq_first = Some((m.sequence, at));
                    }
                    rep.seq_last = Some((m.sequence, at));
                }
                Err(_) => rep.errors += 1,
            },
            op::base::ERROR => {
                let _e: Error = Error::decode(&payload[..]).unwrap_or_else(|_| Error::default());
                rep.errors += 1;
            }
            _ => {}
        }
    }

    // Graceful leave (counted by the server's join/leave metrics) and
    // wait for the ack: without it, the socket close — and any server
    // shutdown that follows — can race ahead of the leave, and the
    // server never counts it.
    let f = frame(op::base::LEAVE_ROOM_REQ, &LeaveRoom {}.encode_to_vec());
    rep.bytes_out += f.len() as u64;
    if w.write_all(&f).await.is_ok() && w.flush().await.is_ok() {
        let leave_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < leave_deadline {
            let timeout = leave_deadline.saturating_duration_since(Instant::now());
            let got = tokio::time::timeout(timeout, read_frame(&mut r))
                .await
                .ok()
                .flatten();
            let Some((op, payload)) = got else {
                break;
            };
            rep.bytes_in += (4 + 2 + payload.len()) as u64;
            if op == op::base::LEAVE_ROOM_RESULT {
                let _ = LeaveRoomResult::decode(&payload[..]);
                rep.left = true;
                break;
            }
        }
    }
    rep
}

/// The client's measured server tick rate (the snapshot sequence is the
/// global tick index).
fn measured_hz(r: &ClientReport) -> Option<f64> {
    let ((f, t1), (l, t2)) = (r.seq_first?, r.seq_last?);
    if l <= f {
        return None;
    }
    let dt = t2.duration_since(t1).as_secs_f64();
    if dt < 0.5 {
        return None;
    }
    Some((l - f) as f64 / dt)
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[s.len() / 2]
}

fn pctl(v: &mut [u128], p: f64) -> u128 {
    if v.is_empty() {
        return 0;
    }
    v.sort();
    v[(v.len() as f64 * p).min(v.len() as f64 - 1.0) as usize]
}

/// Approximate percentile of a step-duration histogram (bin midpoints;
/// the top bin uses the observed max). The bins are fractions of the room's
/// tick budget (`budget_us`), so the result is in µs and the overflow
/// boundary (the budget) is meaningful.
fn hist_percentile(hist: &[u64], budget_us: u64, max_us: u64, p: f64) -> f64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let target = total as f64 * p;
    let mut acc = 0u64;
    for (i, &n) in hist.iter().enumerate() {
        acc += n;
        if acc as f64 >= target {
            let lo = if i == 0 { 0 } else { hist_edge_us(budget_us, i - 1) };
            let hi = if i < hist.len() - 1 {
                hist_edge_us(budget_us, i)
            } else {
                max_us.max(lo + 1)
            };
            return (lo as f64 + hi as f64) / 2.0;
        }
    }
    max_us as f64
}

/// Fraction of steps that exceed the tick budget (bins >= HIST_OVERFLOW_BIN),
/// i.e. the "room cannot keep its rate" mass — now readable from the
/// budget-relative histogram (A1).
fn over_budget_frac(hist: &[u64]) -> f64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0.0;
    }
    hist.iter().skip(HIST_OVERFLOW_BIN).sum::<u64>() as f64 / total as f64
}

struct InProcessServer {
    handle: gsb_server::ServerHandle,
    rep_rx: mpsc::UnboundedReceiver<MetricReport>,
}

/// Start the server in-process with a channel metrics sink. The receiver
/// moves into the report-drain task; nothing is shared across tasks
/// beyond that mailbox. `aoi`/`cell_size` select the room group key (the
/// AOI-off baseline vs the spatial AOI path).
async fn start_inprocess(
    aoi: bool,
    cell_size: f32,
    max_snapshot_bytes: usize,
) -> Result<InProcessServer, gsb_server::ServerError> {
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        aoi,
        aoi_cell_size: cell_size,
        max_snapshot_bytes,
        ..Default::default()
    };
    let (rep_tx, rep_rx) = mpsc::unbounded_channel::<MetricReport>();
    let handle = gsb_server::start_server_metrics(cfg, rep_tx).await?;
    Ok(InProcessServer { handle, rep_rx })
}

/// Own the report receiver in a dedicated task (its only awaited source
/// is the channel); keep every report — the peak gauges (connection
/// count) and a stable tick rate need the series, not just the last
/// (shutdown) report.
async fn drain_reports(mut rx: mpsc::UnboundedReceiver<MetricReport>) -> Vec<MetricReport> {
    let mut all = Vec::new();
    while let Some(r) = rx.recv().await {
        all.push(r);
    }
    all
}

async fn run(args: Args) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    // The in-process handle stays with the main task: it must be stopped
    // only AFTER the clients are done, so it cannot be spawned early.
    let (addr, inproc, rep_rx, server) = match &args.addr {
        Some(addr) => {
            let a: SocketAddr = addr.parse().expect("valid --addr HOST:PORT");
            eprintln!("mode: external server at {a} (client-side numbers only)");
            (a, false, None, None)
        }
        None => {
            let s = start_inprocess(
                args.aoi,
                args.cell_size,
                args.max_snapshot_bytes,
            )
            .await
            .expect("server starts");
            let addr = s.handle.addr;
            eprintln!("mode: in-process server at {addr} (clients share CPU with server)");
            (addr, true, Some(s.rep_rx), Some(s.handle))
        }
    };

    eprintln!(
        "clients={} room={} duration={}s move_ms={} stagger_ms={} aoi={} cell_size={} max_snap_bytes={}",
        args.clients,
        args.room,
        args.duration.as_secs(),
        args.move_ms.as_millis(),
        args.stagger_ms,
        if args.aoi { "on" } else { "off" },
        if args.aoi {
            args.cell_size.to_string()
        } else {
            "-".to_string()
        },
        args.max_snapshot_bytes
    );

    // Spawn the report drain (one task, one awaited source), then the N
    // clients. The main task collects client results with plain
    // sequential awaits — the clients all run in parallel anyway.
    let drain = rep_rx.map(|rx| tokio::spawn(drain_reports(rx)));
    let deadline = Instant::now() + args.duration;
    let mut clients = Vec::with_capacity(args.clients as usize);
    for i in 0..args.clients {
        clients.push(tokio::spawn(run_client(
            i,
            addr,
            args.room,
            args.move_ms,
            args.stagger_ms,
            deadline,
        )));
    }
    let mut reports = Vec::with_capacity(clients.len());
    for h in clients {
        reports.push(h.await.expect("client task panicked"));
    }
    // Small grace period: let the leave acks, the connection actors'
    // final metric flushes, and the registry's leave flushes settle.
    tokio::time::sleep(Duration::from_millis(150)).await;

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

    print_report(&args, inproc, &reports, &server_reports);
}

fn print_report(
    args: &Args,
    inproc: bool,
    reports: &[ClientReport],
    server_reports: &[MetricReport],
) {
    // The report with the highest cumulative step count (cumulative
    // counters are monotonic, so this is the latest room state; the
    // shutdown emit carries the same cumulative values).
    let last_room = server_reports
        .iter()
        .filter(|r| !r.rooms.is_empty())
        .max_by_key(|r| r.rooms[0].steps);
    // Peak registered connections across the whole run (the final report
    // is post-teardown, so its gauges are ~0).
    let peak_conns = server_reports
        .iter()
        .filter_map(|r| r.registry.map(|g| g.conns))
        .max()
        .unwrap_or(0);
    // Stable measured server tick rate: the median over all reports'
    // per-window rates (excluding the first report's empty window and
    // any narrow shutdown window).
    let server_hz = median(
        &server_reports
            .iter()
            .filter(|r| !r.rooms.is_empty())
            .map(|r| r.rooms[0].hz)
            .filter(|h| *h > 0.0)
            .collect::<Vec<_>>(),
    );
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };

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
    let hzs: Vec<f64> = reports.iter().filter_map(measured_hz).collect();
    let hz_med = median(&hzs);
    let dur = args.duration.as_secs_f64().max(1e-9);

    println!("=== gsb loadgen raw report ===");
    println!(
        "machine: cores={cores} profile={profile} mode={}",
        if inproc {
            "in-process (clients share CPU with server)"
        } else {
            "external server"
        }
    );
    println!(
        "clients: connected={connected}/{} joined={joined} left={left} errors={errors}",
        args.clients
    );
    let slowest = reports
        .iter()
        .filter(|r| r.connected)
        .max_by_key(|r| r.connect_ms);
    println!(
        "connect: p50={}ms p99={}ms slowest={}ms (client #{})",
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

    let room = last_room.and_then(|l| l.rooms.first());
    let net = last_room.map(|l| &l.net);
    // Server-side bytes-out per connection (the AOI signal: AOI lowers this).
    let out_bps_per_conn = net
        .map(|n| (n.bytes_out_total as f64 / dur) / connected.max(1) as f64)
        .unwrap_or(0.0);
    if let Some(r) = room {
        println!(
            "server room (final): steps={} hz={:.2} budget_us={} step_min_us={} step_mean_us={:.1} step_max_us={} step_p50_us~{:.0} step_p99_us~{:.0} over_budget={:.1}% hist=[{}]",
            r.steps,
            r.hz,
            r.budget_us,
            r.step_min_us,
            r.step_mean_us,
            r.step_max_us,
            hist_percentile(&r.step_hist, r.budget_us, r.step_max_us, 0.50),
            hist_percentile(&r.step_hist, r.budget_us, r.step_max_us, 0.99),
            over_budget_frac(&r.step_hist) * 100.0,
            r.step_hist
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        println!(
            "server room (final): late_max_us={} lagged_events={} lagged_ticks={} dropped={} keepalive_resends={} snapshots={} max_payload_b={} snap_overflows={} out_bps_per_conn={:.0} groups={} members={} max_group={} joins={} leaves={} metrics_dropped={}",
            r.late_max_us,
            r.lagged_events,
            r.lagged_ticks,
            r.dropped,
            r.keepalive_resends,
            r.snapshots,
            r.snap_bytes_max,
            r.snap_overflows,
            out_bps_per_conn,
            r.groups,
            r.members,
            r.max_group,
            r.joins,
            r.leaves,
            r.metrics_dropped
        );
        if let Some(g) = &last_room.and_then(|l| l.registry) {
            println!(
                "server registry (final): rooms={} conns={} opens={} closes={} joins={} leaves={} peak_conns={}",
                g.rooms, g.conns, g.opens, g.closes, g.joins, g.leaves, peak_conns
            );
        }
        if let Some(n) = net {
            println!(
                "server net (final): bytes_in={} KB ({} KB/s) bytes_out={} KB ({} KB/s) frames_in={} frames_out={}",
                n.bytes_in / 1024,
                n.bytes_in as f64 / 1024.0 / dur,
                n.bytes_out_total / 1024,
                n.bytes_out_total as f64 / 1024.0 / dur,
                n.frames_in,
                n.frames_out
            );
        }
    } else {
        println!("server metrics: unavailable (external mode)");
    }

    // Machine-parseable summary (consumed by tests/loadgen_smoke.rs).
    println!(
        "RESULT mode={} aoi={} max_snap_bytes={} clients={} connected={} joined={} left={} snap_total={} \
         snap_per_client_p50={:.1} tick_hz_med={:.2} client_in_bps={} client_out_bps={} \
         out_bps_per_conn={:.0} moves={} errors={} steps={} server_hz={:.2} \
         step_p50_us={:.0} step_max_us={} step_over_budget_pct={:.1} dropped={} late_max_us={} \
         peak_payload_b={} snap_overflows={} server_in_bps={} server_out_bps={} peak_conns={} \
         metrics_dropped={}",
        if inproc { "in-proc" } else { "ext" },
        if args.aoi { "on" } else { "off" },
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
        room.map(|r| r.steps).unwrap_or(0),
        server_hz,
        room
            .map(|r| hist_percentile(&r.step_hist, r.budget_us, r.step_max_us, 0.50))
            .unwrap_or(0.0),
        room.map(|r| r.step_max_us).unwrap_or(0),
        room.map(|r| over_budget_frac(&r.step_hist) * 100.0).unwrap_or(0.0),
        room.map(|r| r.dropped).unwrap_or(0),
        room.map(|r| r.late_max_us).unwrap_or(0),
        room.map(|r| r.snap_bytes_max as u64).unwrap_or(0),
        room.map(|r| r.snap_overflows).unwrap_or(0),
        net
            .map(|n| (n.bytes_in as f64 / dur) as u64)
            .unwrap_or(0),
        net
            .map(|n| (n.bytes_out_total as f64 / dur) as u64)
            .unwrap_or(0),
        peak_conns,
        last_room.map(|l| l.metrics_dropped).unwrap_or(0),
    );
}

fn main() {
    let args = parse_args();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(run(args));
}
