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
//! gsb-loadgen [N] [--duration SECS] [--move-ms MS] [--room ID] [--stagger-ms MS]
//!              [--visibility all|spatial|team|pvs] [--cell-size N] [--vision-radius N]
//!              [--max-snapshot-bytes N] [--addr HOST:PORT]
//! ```
//! Defaults: N=100, duration=10 s, move interval 150 ms, room 1,
//! stagger 0 (all clients connect at once — the worst case for the accept
//! path over loopback; see `Args::stagger_ms`), visibility `all`
//! (same default as the server config; see `gsb_server::Visibility`).
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
use std::process::Stdio;
use std::time::{Duration, Instant};

use gsb_protocol::base::{
    Auth, Error, JoinRoom, JoinRoomResult, LeaveRoom, LeaveRoomResult,
};
use gsb_protocol::op;
use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, tcp::OwnedReadHalf};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc;

use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::{
    hist_edge_us, MetricReport, NetReport, RegistryReport, RoomReport, HIST_BINS,
    HIST_OVERFLOW_BIN,
};

/// The client movement profile (the load's *shape*, not the server's
/// visibility strategy):
///
/// - [`Profile::Ring`] (default, the historical profile): every client
///   chases a point circling a radius-40 circle at 4 rad/s, phase-shifted
///   by its id. The targets outrun the entities (160 vs 10 u/s), so the
///   entities crowd the central band of the arena — a *clustered*
///   layout. Kept unchanged (default) so all previous measurements stay
///   comparable.
/// - [`Profile::Spread`]: every client wanders in a small circle around a
///   deterministic *home* point, uniformly distributed over the
///   `[-H, H]²` map (H = `--spawn-half-size`, default 1000 ⇒ a 2000×2000
///   "wide map"). The layout stays ~uniform over the whole run (the server
///   spawns entities with the same distribution, so there is no migration
///   artifact), which is the *sparse* layout a real MOBA arena resembles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    Ring,
    Spread,
}

impl Profile {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "ring" => Some(Self::Ring),
            "spread" => Some(Self::Spread),
            _ => None,
        }
    }
}

struct Args {
    clients: u64,
    /// First client id; client i is `offset + i` (the stagger and the
    /// profile's per-id determinism use the *global* id, so a partitioned
    /// run of N clients across several processes is identical to one
    /// in-process run of the same N).
    offset: u64,
    duration: Duration,
    move_ms: Duration,
    room: u64,
    /// Client i connects after i × stagger_ms. 0 (default) = all at once.
    /// A burst of N simultaneous connects over loopback hits the tokio
    /// accept path in its worst case (edge-triggered wakeup per state
    /// change; see the load-test report); a stagger models the realistic
    /// trickle of players joining over time. Fractional values allowed
    /// (10k clients × 0.5 ms = 5 s of spread).
    stagger_ms: f64,
    addr: Option<String>,
    /// The movement profile of this process's clients (see [`Profile`]).
    profile: Profile,
    /// The map half-size for the `spread` profile's home distribution
    /// (must match the server's `spawn_half_size` so the spawn and the
    /// homes live on the same map).
    spawn_half: f32,
    /// The visibility strategy of the in-process / served server
    /// (`--visibility all|spatial|team|pvs`, default `all` — same as the
    /// server config default).
    visibility: gsb_server::Visibility,
    /// AOI cell size in world units (`--cell-size N`, default 20; used
    /// only for `spatial`).
    cell_size: f32,
    /// Team-fog vision radius in world units (`--vision-radius N`,
    /// default 25; used only for `team`).
    vision_radius: f32,
    /// Per-snapshot payload ceiling the room enforces
    /// (`--max-snapshot-bytes N`, default 1400 = the rUDP MTU the spec
    /// assumes). The TCP transport default is 1 MiB, so the in-process
    /// server models the MTU-constrained scenario out of the box; `snap_*`
    /// overflow counters are only meaningful against this ceiling.
    max_snapshot_bytes: usize,
    /// The spawn map half-size of the in-process / served server (default
    /// 50 = the historical 100×100 arena, bit-identical behavior).
    server_spawn_half: f32,
    /// Run the server only (no clients): `--serve`. Exits cleanly after
    /// `--duration`. With `--metrics-listen`, the server's metric
    /// *reports* (the same channel sink the in-process mode uses) are
    /// streamed in a small binary format to the single connecting
    /// orchestrator — no stdout parsing.
    serve: bool,
    /// Bind address for `--serve` (default 127.0.0.1:7777).
    bind: String,
    /// Where `--serve` accepts the orchestrator's metrics connection.
    metrics_listen: Option<String>,
    /// Orchestrator mode: spawn one server process (`--serve`) and
    /// `--procs` client processes (this binary, `--addr` mode), collect
    /// the server's metric reports over the metrics socket, merge the
    /// clients' per-client records, and print the single final report.
    /// This is the separate-process mode: the server runs in its own
    /// process (and, with `--pin`, on disjoint cores), so its CPU is
    /// isolated from the clients' decode work.
    orchestrate: bool,
    /// Per-room membership cap of the in-process / served server
    /// (`--max-players N`, 0 = unlimited). A join into a full room is
    /// rejected with `ERROR` code 8 (the client stays connected; its
    /// `joined` stays false and it counts a `join_rejected`).
    max_players: Option<u32>,
    /// Server-wide connection cap of the in-process / served server
    /// (`--max-connections N`, 0 = unlimited). New connections beyond it
    /// are rejected at birth with `ERROR` code 9 + EOF (the client counts
    /// a `cap_rejected`).
    max_connections: Option<u64>,
    /// Session-lifecycle idle window of the in-process / served server, in
    /// seconds (`--idle-timeout-secs F`; 0 = disabled; unspecified = the
    /// server config default, 30 s).
    idle_timeout_secs: Option<f64>,
    /// The GLOBAL id of the client that floods (`--flood-id K`): after
    /// joining it writes MOVE_TO frames in a tight loop (as fast as the
    /// socket accepts) until the deadline — the input-flood behaviour
    /// probe for the fairness / drop-attribution guards.
    flood_id: Option<u64>,
    /// Number of client processes in orchestrator mode (default 1).
    procs: u32,
    /// Pin the spawned processes to disjoint core sets with `taskset`
    /// (orchestrator mode): the server gets `--pin-server-cores` physical
    /// cores, the client processes share the rest. Without `taskset` the
    /// orchestrator warns and runs unpinned.
    pin: bool,
    /// How many *physical* cores the server gets under `--pin` (default 8).
    pin_server_cores: u32,
    /// Tokio worker threads of *this* process (0 = runtime default).
    /// The orchestrator sizes each child to its pinned core set.
    workers: usize,
}

/// The usage text (`--help` / `-h`).
const USAGE: &str = "\
gsb-loadgen — load generator for gsb servers (N real-TCP clients)

Usage:
  gsb-loadgen [N] [client options]            run N clients (in-process or
                                               against --addr)
  gsb-loadgen --serve [server options]        run the server only
  gsb-loadgen --orchestrate [N] [options]     one server process + P
                                               client processes, one report

Client options:
  --addr HOST:PORT          connect to an external server (default: start
                            an in-process server)
  --duration SECS           run duration (default 10)
  --move-ms MS              MOVE_TO interval (default 150)
  --room ID                 room id to join (default 1)
  --stagger-ms MS           client i connects i×ms later (default 0)
  --offset K                first client id (default 0; client i = K+i)
  --profile ring|spread     movement profile (default ring — the historical
                            clustered layout; spread = uniform over the
                            ±spawn-half map, the sparse MOBA-like layout)
  --spawn-half-size F       map half-size for the spread profile's homes
                            and the (in-process/served) server's spawn
                            points (default: 50 for ring, 1000 for spread)
  --workers N               tokio worker threads for this process

Server options (in-process server, --serve, or the orchestrator's server):
  --visibility all|spatial|team|pvs   (default all)
  --cell-size F                       (spatial; default 20)
  --vision-radius F                   (team; default 25)
  --max-snapshot-bytes N              (default 1400)
  --max-players N                     per-room membership cap (0 = unlimited;
                                       default: the server config default,
                                       10 000 — the measured single-room wall)
  --max-connections N                 server-wide connection cap (0 =
                                       unlimited; default: the server config
                                       default, 100 000)
  --idle-timeout-secs F               idle session window (0 = disabled;
                                       default: the server config default, 30)

Client behaviour:
  --flood-id K                        client K floods MOVE_TO in a tight loop
                                      after joining (the input-flood probe)

Orchestrator options (--orchestrate):
  --procs P             client process count (default 1)
  --pin                 pin children to disjoint cores via taskset
  --pin-server-cores C  physical cores for the server under --pin (default 8)

Server-only options (--serve):
  --bind HOST:PORT          (default 127.0.0.1:7777)
  --metrics-listen HOST:PORT  stream metric reports (binary, channel data)
                              to one connecting orchestrator; without it,
                              reports go to the gsb-metric log

Misc:
  -h, --help                this text";

fn parse_args() -> Args {
    let mut args = Args {
        clients: 100,
        offset: 0,
        duration: Duration::from_secs(10),
        move_ms: Duration::from_millis(150),
        room: 1,
        stagger_ms: 0.0,
        addr: None,
        profile: Profile::Ring,
        spawn_half: 50.0,
        visibility: gsb_server::Visibility::default(),
        cell_size: 20.0,
        vision_radius: gsb_game::team::DEFAULT_VISION_RADIUS,
        max_snapshot_bytes: 1400,
        server_spawn_half: 50.0,
        serve: false,
        bind: "127.0.0.1:7777".into(),
        metrics_listen: None,
        orchestrate: false,
        procs: 1,
        pin: false,
        pin_server_cores: 8,
        workers: 0,
        max_players: None,
        max_connections: None,
        idle_timeout_secs: None,
        flood_id: None,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].clone();
        i += 1;
        let mut v = || {
            if i < argv.len() {
                let s = argv[i].clone();
                i += 1;
                s
            } else {
                panic!("{a} needs a value (try --help)")
            }
        };
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--duration" => args.duration = Duration::from_secs_f64(v().parse().expect("number")),
            "--move-ms" => args.move_ms = Duration::from_millis(v().parse().expect("number")),
            "--room" => args.room = v().parse().expect("number"),
            "--offset" => args.offset = v().parse().expect("number"),
            "--stagger-ms" => args.stagger_ms = v().parse().expect("number"),
            "--addr" => args.addr = Some(v()),
            "--profile" => {
                args.profile = Profile::parse(&v())
                    .unwrap_or_else(|| panic!("--profile: expected ring|spread (try --help)"))
            }
            "--spawn-half-size" => {
                let s = v().parse().expect("number");
                args.spawn_half = s;
                args.server_spawn_half = s; // one knob: spawn map = home map
            }
            "--visibility" => {
                let s = v();
                args.visibility = match s.as_str() {
                    "all" => gsb_server::Visibility::All,
                    "spatial" => gsb_server::Visibility::Spatial,
                    "team" => gsb_server::Visibility::Team,
                    "pvs" => gsb_server::Visibility::Pvs,
                    other => panic!("--visibility: expected all|spatial|team|pvs, got {other}"),
                };
            }
            "--cell-size" => args.cell_size = v().parse().expect("number"),
            "--vision-radius" => args.vision_radius = v().parse().expect("number"),
            "--max-snapshot-bytes" => args.max_snapshot_bytes = v().parse().expect("number"),
            "--serve" => args.serve = true,
            "--bind" => args.bind = v(),
            "--metrics-listen" => args.metrics_listen = Some(v()),
            "--orchestrate" => args.orchestrate = true,
            "--procs" => args.procs = v().parse().expect("number"),
            "--pin" => args.pin = true,
            "--pin-server-cores" => args.pin_server_cores = v().parse().expect("number"),
            "--workers" => args.workers = v().parse().expect("number"),
            "--max-players" => {
                let n: u32 = v().parse().expect("number");
                args.max_players = Some(n);
            }
            "--max-connections" => {
                let n: u64 = v().parse().expect("number");
                args.max_connections = Some(n);
            }
            "--idle-timeout-secs" => {
                args.idle_timeout_secs = Some(v().parse().expect("number"));
            }
            "--flood-id" => args.flood_id = Some(v().parse().expect("number")),
            s if s.starts_with("--") => panic!("unknown flag {s} (try --help)"),
            s => args.clients = s.parse().expect("N must be a number"),
        }
    }
    // The spread profile's default map (a "wide map": 2000×2000) — the
    // ring profile stays on the historical 100×100 arena, so the default
    // run is bit-identical to the historical one.
    if args.spawn_half == 50.0 && args.profile == Profile::Spread {
        args.spawn_half = 1000.0;
        args.server_spawn_half = 1000.0;
    }
    if args.orchestrate && args.serve {
        panic!("--orchestrate and --serve are mutually exclusive (try --help)");
    }
    if args.serve && args.clients != 100 && args.addr.is_none() {
        // `gsb-loadgen --serve` takes no client count; a bare number
        // before --serve is the N of a client run, so this is a mistake.
        eprintln!("note: --serve ignores the client count (it runs no clients)");
    }
    if !args.serve && !args.orchestrate && args.clients == 0 {
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
    /// Join rejections observed (`ERROR` code 8, room full): the room
    /// capacity guardrail working — the connection stays alive.
    join_rejected: u64,
    /// Connection-capacity rejections observed (`ERROR` code 9): the
    /// server-wide cap rejected this connection at birth.
    cap_rejected: u64,
    /// First/last snapshot sequence with its arrival instant: the server's
    /// measured tick rate is (last_seq − first_seq) / Δt, since the
    /// snapshot sequence is the global tick index.
    seq_first: Option<(u64, Instant)>,
    seq_last: Option<(u64, Instant)>,
}

/// The `spread` profile's deterministic home for client `id`: the SAME
/// lattice the server's `gsb_game::room::spawn_pos` uses (same hash, same
/// scaling), so spawn points and homes live on the same map. The
/// *distribution* is what the profile contributes (uniform over the map,
/// statistically steady from tick 1 — see `Profile::Spread`).
fn spawn_home(id: u64, half: f32) -> (f64, f64) {
    let h = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let scale = half as f64 / 50.0;
    let x = ((h % 1000) as f64 / 10.0 - 50.0) * scale;
    let y = (((h >> 32) % 1000) as f64 / 10.0 - 50.0) * scale;
    (x, y)
}

/// Everything one client task needs besides its own id. (One struct
/// rather than eight scalars — the profile work kept adding fields.)
#[derive(Clone)]
struct ClientParams {
    addr: SocketAddr,
    room: u64,
    move_ms: Duration,
    stagger_ms: f64,
    profile: Profile,
    spawn_half: f32,
    deadline: Instant,
    /// Flood mode (the `--flood-id` client): after joining, write MOVE_TO
    /// in a tight loop until the deadline — the input-flood behaviour
    /// probe for the per-connection pull budget and the drop attribution.
    flood: bool,
}

async fn run_client(id: u64, p: ClientParams) -> ClientReport {
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
        join_rejected: 0,
        cap_rejected: 0,
        seq_first: None,
        seq_last: None,
    };

    // Optional connect stagger (see `Args::stagger_ms`).
    if p.stagger_ms > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(id as f64 * p.stagger_ms / 1000.0)).await;
    }
    let t0 = Instant::now();
    let stream = match TcpStream::connect(p.addr).await {
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
    out.extend(frame(op::base::JOIN_ROOM_REQ, &JoinRoom { room_id: p.room }.encode_to_vec()));
    rep.bytes_out += out.len() as u64;
    if w.write_all(&out).await.is_err() || w.flush().await.is_err() {
        return rep;
    }

    let t_start = Instant::now();
    let mut last_move = t_start;
    let mut flooded = false;
    loop {
        let now = Instant::now();
        if now >= p.deadline {
            break;
        }
        if now.duration_since(last_move) >= p.move_ms {
            last_move = now;
            let (tx, ty) = match p.profile {
                // The historical profile (UNCHANGED — all previous
                // measurements stay comparable): a circle of radius 40
                // around the map center at 4 rad/s, phase-shifted per
                // client (id-based offset so N entities do not move in
                // lockstep). The targets outrun the entities, so the
                // entities crowd the central band — the *clustered*
                // layout.
                Profile::Ring => {
                    let angle =
                        (now.duration_since(t_start).as_secs_f64() + id as f64 * 0.618) * 4.0;
                    (angle.cos() * 40.0, angle.sin() * 40.0)
                }
                // The *spread* profile: each client wanders a small
                // circle (radius 20, 0.4 rad/s — the target stays
                // reachable at 10 u/s, so the entity tracks it closely)
                // around its deterministic home, uniform over the
                // ±spawn_half map. The layout stays ~uniform over the
                // whole run (the server spawns with the same
                // distribution), so there is no migration artifact and
                // the run is statistically steady from tick 1. This is
                // the sparse, wide-map layout a real MOBA arena
                // resembles — where team fog actually hides most
                // enemies (the clustered profile hides none).
                Profile::Spread => {
                    let (hx, hy) = spawn_home(id, p.spawn_half);
                    let w = (now.duration_since(t_start).as_secs_f64() + id as f64 * 0.618) * 0.4;
                    (hx + w.cos() * 20.0, hy + w.sin() * 20.0)
                }
            };
            let msg = gsb_game::game::MoveTo {
                x: tx as i32,
                y: ty as i32,
            };
            let f = frame(gsb_game::op::MOVE_TO, &msg.encode_to_vec());
            rep.moves += 1;
            rep.bytes_out += f.len() as u64;
            if w.write_all(&f).await.is_err() || w.flush().await.is_err() {
                break; // peer gone
            }
        }
        let timeout = p.deadline
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
                if p.flood {
                    // Flood mode: leave the paced loop; the tight write
                    // loop below runs until the deadline.
                    flooded = true;
                    break;
                }
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
                let e: Error = Error::decode(&payload[..]).unwrap_or_else(|_| Error::default());
                match e.code {
                    // The capacity guardrails, observed from the client
                    // side: 8 = room full (gentle reject, connection
                    // stays), 9 = server at connection capacity.
                    8 => rep.join_rejected += 1,
                    9 => rep.cap_rejected += 1,
                    _ => rep.errors += 1,
                }
            }
            _ => {}
        }
    }

    if flooded {
        // The input flood: write MOVE_TO as fast as the socket accepts,
        // until the deadline. The server-side chain (reader pump → conn
        // inbox → conn actor → action channel → room pull budget) bounds
        // what actually reaches the tick; the excess is dropped on the
        // flooder's OWN full action channel (attributed to it).
        let msg = gsb_game::game::MoveTo { x: 0, y: 0 };
        let f = frame(gsb_game::op::MOVE_TO, &msg.encode_to_vec());
        while Instant::now() < p.deadline {
            if w.write_all(&f).await.is_err() {
                break; // peer gone
            }
            rep.moves += 1;
            rep.bytes_out += f.len() as u64;
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

/// Capacity / lifecycle probe overrides for the in-process / served
/// server. Each `None` = unspecified (keep the server config default);
/// an explicit `0` = unlimited (rooms) / disabled (idle window).
struct ServerOverrides {
    max_players: Option<u32>,
    max_connections: Option<u64>,
    idle_timeout_secs: Option<f64>,
}

fn apply_overrides(cfg: &mut gsb_server::Config, o: &ServerOverrides) {
    if let Some(n) = o.max_players {
        cfg.max_players = (n != 0).then_some(n);
    }
    if let Some(n) = o.max_connections {
        cfg.max_connections = (n != 0).then_some(n);
    }
    if let Some(s) = o.idle_timeout_secs {
        cfg.idle_timeout_secs = s;
    }
}

/// Start the server in-process with a channel metrics sink. The receiver
/// moves into the report-drain task; nothing is shared across tasks
/// beyond that mailbox. `visibility` selects the room group key (the
/// four strategies: `()` / `Cell` / `Team` / `Sector`).
async fn start_inprocess(
    visibility: gsb_server::Visibility,
    cell_size: f32,
    vision_radius: f32,
    max_snapshot_bytes: usize,
    spawn_half: f32,
    overrides: ServerOverrides,
) -> Result<InProcessServer, gsb_server::ServerError> {
    let mut cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        visibility,
        aoi_cell_size: cell_size,
        team_vision_radius: vision_radius,
        max_snapshot_bytes,
        spawn_half_size: spawn_half,
        ..Default::default()
    };
    apply_overrides(&mut cfg, &overrides);
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

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
}

async fn run(args: Args) {
    init_tracing();

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
                args.visibility,
                args.cell_size,
                args.vision_radius,
                args.max_snapshot_bytes,
                args.server_spawn_half,
                ServerOverrides {
                    max_players: args.max_players,
                    max_connections: args.max_connections,
                    idle_timeout_secs: args.idle_timeout_secs,
                },
            )
            .await
            .expect("server starts");
            let addr = s.handle.addr;
            eprintln!("mode: in-process server at {addr} (clients share CPU with server)");
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
            Profile::Ring => "ring",
            Profile::Spread => "spread",
        }
    );

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
        room: args.room,
        move_ms: args.move_ms,
        stagger_ms: args.stagger_ms,
        profile: args.profile,
        spawn_half: args.spawn_half,
        deadline,
        flood: false,
    };
    let mut clients = Vec::with_capacity(n as usize);
    for i in 0..n {
        let id = args.offset + i;
        // The `--flood-id` client (by GLOBAL id) runs the tight-write flood
        // after joining; every other client is paced normally.
        p.flood = args.flood_id == Some(id);
        clients.push(tokio::spawn(run_client(id, p.clone())));
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
                 bytes_in={} bytes_out={} moves={} errors={} join_rejected={} cap_rejected={} hz={}",
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
                match measured_hz(r) {
                    Some(h) => format!("{h:.3}"),
                    None => "-".to_string(),
                }
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

/// Extra facts of a separate-process (orchestrated) run; `None` for the
/// in-process and external-direct modes.
struct SepInfo {
    procs: u32,
    server_pid: u32,
    client_pids: Vec<u32>,
    /// The disjoint core sets (`taskset` masks), for the record: e.g.
    /// `server:0,1,2,3;client0:4,5,6,7;client1:8,9,10,11` — or `none`
    /// when pinning was not possible.
    affinity: String,
    /// CPU seconds the server process used over the run (from
    /// /proc/<pid>/stat) — the isolation proof: with disjoint masks,
    /// server CPU cannot hide behind client decode.
    server_cpu_s: f64,
    clients_cpu_s: f64,
}

fn print_report(
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
    // Peak room membership (same rationale): the stable entity count for
    // the overlap ratio.
    let peak_members = server_reports
        .iter()
        .filter_map(|r| r.rooms.first().map(|r| r.members))
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
    let base = server_reports
        .iter()
        .find(|r| !r.rooms.is_empty() && r.rooms[0].steps >= 100)
        .and_then(|r| r.rooms.first());
    let last_steady = server_reports
        .iter()
        .filter(|r| !r.rooms.is_empty() && r.rooms[0].members == peak_members)
        .max_by_key(|r| r.rooms[0].steps)
        .or(last_room)
        .and_then(|r| r.rooms.first());
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
    let join_rejected: u64 = reports.iter().map(|r| r.join_rejected).sum();
    let cap_rejected: u64 = reports.iter().map(|r| r.cap_rejected).sum();
    let hz_med = median(hzs);
    let dur = args.duration.as_secs_f64().max(1e-9);

    println!("=== gsb loadgen raw report ===");
    println!(
        "machine: cores={cores} profile={profile} mode={} clients_span={}..{}",
        mode_human,
        args.offset,
        args.offset + args.clients.saturating_sub(1)
    );
    println!(
        "clients: connected={connected}/{} joined={joined} left={left} errors={errors} join_rejected={join_rejected} cap_rejected={cap_rejected}",
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
        // `hz` here is the run's *median* measured rate (the same value the
        // RESULT line reports as server_hz), NOT the final report's
        // window rate: the final report can be a 0-sample window (the
        // shutdown emit lands before the room's next 1 Hz sample, so its
        // Δsteps is 0 and its window rate reads 0.00) — printing that
        // would contradict the RESULT line in exactly the shutdown case
        // where a reader needs the two to agree.
        println!(
            "server room (final): steps={} hz={:.2} budget_us={} step_min_us={} step_mean_us={:.1} step_max_us={} step_p50_us~{:.0} step_p99_us~{:.0} over_budget={:.1}% hist=[{}]",
            r.steps,
            server_hz,
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
        "RESULT mode={} visibility={} max_snap_bytes={} clients={} connected={} joined={} left={} snap_total={} \
         snap_per_client_p50={:.1} tick_hz_med={:.2} client_in_bps={} client_out_bps={} \
         out_bps_per_conn={:.0} moves={} errors={} steps={} server_hz={:.2} \
         step_p50_us={:.0} step_max_us={} step_over_budget_pct={:.1} dropped={} late_max_us={} \
         peak_payload_b={} snap_overflows={} records_per_tick={:.1} overlap_x={:.2} \
         server_in_bps={} server_out_bps={} peak_conns={} metrics_dropped={} \
         profile={} offset={} procs={} server_pid={} client_pids={} affinity={} \
         server_cpu_s={:.1} clients_cpu_s={:.1} \
          join_rejected={} cap_rejected={} actions_dropped={} actions_dropped_top={}",
        mode,
        args.visibility,
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
        rec_per_tick,
        overlap,
        net
            .map(|n| (n.bytes_in as f64 / dur) as u64)
            .unwrap_or(0),
        net
            .map(|n| (n.bytes_out_total as f64 / dur) as u64)
            .unwrap_or(0),
        peak_conns,
        last_room.map(|l| l.metrics_dropped).unwrap_or(0),
        match args.profile {
            Profile::Ring => "ring",
            Profile::Spread => "spread",
        },
        args.offset,
        sep.map(|s| s.procs).unwrap_or(1),
        sep.map(|s| s.server_pid).unwrap_or(0),
        sep
            .map(|s| s.client_pids.iter().map(u32::to_string).collect::<Vec<_>>().join(","))
            .unwrap_or_else(|| "0".to_string()),
        sep.map(|s| s.affinity.clone()).unwrap_or_else(|| "none".to_string()),
        sep.map(|s| s.server_cpu_s).unwrap_or(0.0),
        sep.map(|s| s.clients_cpu_s).unwrap_or(0.0),
        join_rejected,
        cap_rejected,
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
    );
}

fn main() {
    let args = parse_args();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(args.workers.max(1))
        .build()
        .expect("runtime");
    // The three modes (module docs):
    //  - `--serve`: the server process (no clients);
    //  - `--orchestrate`: the orchestrator (spawns server + client
    //    processes, one final report);
    //  - otherwise: N clients in this process (against an in-process or
    //    `--addr` external server).
    if args.serve {
        rt.block_on(serve(args));
    } else if args.orchestrate {
        rt.block_on(orchestrate(args));
    } else {
        rt.block_on(run(args));
    }
}

// ════════════════════════════════════════════════════════════════════════
// Item A — separate-process mode: server process, orchestrator, and the
// binary wire for the server's metric reports.
// ════════════════════════════════════════════════════════════════════════

/// Metric-report wire format (server process → orchestrator, one TCP
/// connection). The *data* is exactly what the in-process mode already
/// receives through the channel sink (`MetricReport` structs, 1 Hz) —
/// this is a serialization of those structs over a socket, **not** a log
/// format: no parsing of human text, no dependency on `gsb-metric` line
/// layout. Little-endian, no padding, one frame per report:
///
/// ```text
/// [u32 magic = 0x47534D31 "GSM1"][u32 body_len][body]
///
/// body =
///   u64 metrics_dropped
///   u32 n_rooms
///   per room (order as in `MetricReport::rooms`):
///     u64 room_id  u64 steps  f64 hz  u64 budget_us  u64 step_min_us
///     f64 step_mean_us  u64 step_max_us  [u64; HIST_BINS] step_hist
///     u64 late_min_us  f64 late_mean_us  u64 late_max_us
///     u64 lagged_events  u64 lagged_ticks  u64 dropped  f64 dropped_s
///     u64 dropped_actions  u64 keepalive_resends  u64 snapshots
///     f64 snap_bytes_s  u32 snap_bytes_max  u64 snap_overflows
///     u64 snap_records  u64 shipped_bytes  f64 shipped_s
///     u32 groups  u32 members  u32 max_group  u64 joins  u64 leaves
///     u64 metrics_dropped
///   u8 registry_present
///   [if present] u32 rooms  u32 conns  u64 rooms_created
///                u64 rooms_destroyed  u64 joins  u64 leaves
///                u64 opens  u64 closes
///   u64 bytes_in  u64 bytes_out_room  u64 bytes_out_control
///   u64 bytes_out_total  u64 frames_in  u64 frames_out
///   u64 actions_dropped
///   u32 n_top  [per entry] u64 conn_id  u64 count
/// ```
///
/// Both directions live in this binary (the server's `--serve` mode and
/// the orchestrator are the same executable), so the format cannot
/// drift between sides; the magic guards against a stale/reordered
/// connection. GSM2 = the GSM1 layout plus the net-scope
/// `actions_dropped` total and its per-connection attribution tail
/// (worst offenders first, ≤ 5 — see `MetricReport::actions_dropped_top`).
const METRICS_MAGIC: u32 = 0x4753_4D32;

/// Little-endian writer (the encode side of the format above).
struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
}

fn encode_report(r: &MetricReport) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(128 + r.rooms.len() * 256));
    w.u64(r.metrics_dropped);
    w.u32(r.rooms.len() as u32);
    for room in &r.rooms {
        w.u64(room.room.0);
        w.u64(room.steps);
        w.f64(room.hz);
        w.u64(room.budget_us);
        w.u64(room.step_min_us);
        w.f64(room.step_mean_us);
        w.u64(room.step_max_us);
        for bin in &room.step_hist {
            w.u64(*bin);
        }
        w.u64(room.late_min_us);
        w.f64(room.late_mean_us);
        w.u64(room.late_max_us);
        w.u64(room.lagged_events);
        w.u64(room.lagged_ticks);
        w.u64(room.dropped);
        w.f64(room.dropped_s);
        w.u64(room.dropped_actions);
        w.u64(room.keepalive_resends);
        w.u64(room.snapshots);
        w.f64(room.snap_bytes_s);
        w.u32(room.snap_bytes_max);
        w.u64(room.snap_overflows);
        w.u64(room.snap_records);
        w.u64(room.shipped_bytes);
        w.f64(room.shipped_s);
        w.u32(room.groups);
        w.u32(room.members);
        w.u32(room.max_group);
        w.u64(room.joins);
        w.u64(room.leaves);
        w.u64(room.metrics_dropped);
    }
    w.u8(match &r.registry {
        Some(_) => 1,
        None => 0,
    });
    if let Some(g) = &r.registry {
        w.u32(g.rooms);
        w.u32(g.conns);
        w.u64(g.rooms_created);
        w.u64(g.rooms_destroyed);
        w.u64(g.joins);
        w.u64(g.leaves);
        w.u64(g.opens);
        w.u64(g.closes);
    }
    w.u64(r.net.bytes_in);
    w.u64(r.net.bytes_out_room);
    w.u64(r.net.bytes_out_control);
    w.u64(r.net.bytes_out_total);
    w.u64(r.net.frames_in);
    w.u64(r.net.frames_out);
    w.u64(r.net.actions_dropped);
    w.u32(r.actions_dropped_top.len() as u32);
    for (conn, n) in &r.actions_dropped_top {
        w.u64(conn.0);
        w.u64(*n);
    }
    let mut frame = Vec::with_capacity(8 + w.0.len());
    frame.extend_from_slice(&METRICS_MAGIC.to_le_bytes());
    frame.extend_from_slice(&(w.0.len() as u32).to_le_bytes());
    frame.extend_from_slice(&w.0);
    frame
}

/// Bounds-checked reader (the decode side).
struct R<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.i.checked_add(n)?;
        if end > self.b.len() {
            return None;
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    }
    fn f64(&mut self) -> Option<f64> {
        self.take(8).map(|s| f64::from_le_bytes(s.try_into().unwrap()))
    }
    fn done(&self) -> bool {
        self.i == self.b.len()
    }
}

fn decode_report(body: &[u8]) -> Option<MetricReport> {
    let mut r = R::new(body);
    let metrics_dropped = r.u64()?;
    let n = r.u32()?;
    let mut rooms = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let room_id = r.u64()?;
        let steps = r.u64()?;
        let hz = r.f64()?;
        let budget_us = r.u64()?;
        let step_min_us = r.u64()?;
        let step_mean_us = r.f64()?;
        let step_max_us = r.u64()?;
        // (the read order must mirror the encode order above)
        let step_hist = {
            let mut h = [0u64; HIST_BINS];
            for bin in &mut h {
                *bin = r.u64()?;
            }
            h
        };
        rooms.push(RoomReport {
            room: RoomId(room_id),
            steps,
            hz,
            budget_us,
            step_min_us,
            step_mean_us,
            step_max_us,
            step_hist,
            late_min_us: r.u64()?,
            late_mean_us: r.f64()?,
            late_max_us: r.u64()?,
            lagged_events: r.u64()?,
            lagged_ticks: r.u64()?,
            dropped: r.u64()?,
            dropped_s: r.f64()?,
            dropped_actions: r.u64()?,
            keepalive_resends: r.u64()?,
            snapshots: r.u64()?,
            snap_bytes_s: r.f64()?,
            snap_bytes_max: r.u32()?,
            snap_overflows: r.u64()?,
            snap_records: r.u64()?,
            shipped_bytes: r.u64()?,
            shipped_s: r.f64()?,
            groups: r.u32()?,
            members: r.u32()?,
            max_group: r.u32()?,
            joins: r.u64()?,
            leaves: r.u64()?,
            metrics_dropped: r.u64()?,
        });
    }
    let registry = match r.u8()? {
        1 => Some(RegistryReport {
            rooms: r.u32()?,
            conns: r.u32()?,
            rooms_created: r.u64()?,
            rooms_destroyed: r.u64()?,
            joins: r.u64()?,
            leaves: r.u64()?,
            opens: r.u64()?,
            closes: r.u64()?,
        }),
        0 => None,
        _ => return None,
    };
    let net = NetReport {
        bytes_in: r.u64()?,
        bytes_out_room: r.u64()?,
        bytes_out_control: r.u64()?,
        bytes_out_total: r.u64()?,
        frames_in: r.u64()?,
        frames_out: r.u64()?,
        actions_dropped: r.u64()?,
    };
    let n_top = r.u32()?;
    let mut actions_dropped_top = Vec::with_capacity(n_top as usize);
    for _ in 0..n_top {
        actions_dropped_top.push((ConnectionId(r.u64()?), r.u64()?));
    }
    if !r.done() {
        return None;
    }
    Some(MetricReport {
        metrics_dropped,
        rooms,
        registry,
        net,
        actions_dropped_top,
    })
}

/// The server process (`--serve`): the same server the in-process mode
/// runs, as a standalone process. Without `--metrics-listen`, reports go
/// to the `gsb-metric` log (RUST_LOG=info) like `gsb-server`; with it,
/// the collector's channel sink feeds one TCP connection — the
/// orchestrator — in the binary format above (the channel path is
/// preserved end-to-end; nothing is parsed from stdout).
async fn serve(args: Args) {
    init_tracing();
    let mut cfg = gsb_server::Config {
        bind: args.bind.clone(),
        room_count: 1,
        visibility: args.visibility,
        aoi_cell_size: args.cell_size,
        team_vision_radius: args.vision_radius,
        max_snapshot_bytes: args.max_snapshot_bytes,
        spawn_half_size: args.server_spawn_half,
        ..Default::default()
    };
    // Capacity / lifecycle overrides (same semantics as in-process: an
    // explicit 0 means unlimited / disabled).
    apply_overrides(
        &mut cfg,
        &ServerOverrides {
            max_players: args.max_players,
            max_connections: args.max_connections,
            idle_timeout_secs: args.idle_timeout_secs,
        },
    );
    match args.metrics_listen {
        Some(listen) => {
            let listen: SocketAddr =
                listen.parse().expect("valid --metrics-listen HOST:PORT");
            let (tx, rx) = mpsc::unbounded_channel::<MetricReport>();
            let handle = gsb_server::start_server_metrics(cfg, tx)
                .await
                .expect("server starts");
            eprintln!(
                "serve: ready at {} (visibility={}, spawn_half={}, duration={}s; metric reports → {})",
                handle.addr,
                args.visibility,
                args.server_spawn_half,
                args.duration.as_secs(),
                listen
            );
            // The export task owns the report receiver (its only awaited
            // sources: the accept, then the channel). The main task's
            // awaited sources: the duration sleep, then stop().
            let export = tokio::spawn(metrics_export(rx, listen));
            tokio::time::sleep(args.duration).await;
            // Clean stop: the collector emits its final report when the
            // ticker's broadcast closes, then drops the channel sender —
            // the export task drains the final report and exits.
            handle.stop().await;
            if let Err(e) = export.await {
                eprintln!("serve: metrics export task failed: {e}");
            }
        }
        None => {
            let handle = gsb_server::start_server(cfg).await.expect("server starts");
            eprintln!(
                "serve: ready at {} (visibility={}, duration={}s; metric reports → gsb-metric log, RUST_LOG=info)",
                handle.addr,
                args.visibility,
                args.duration.as_secs()
            );
            tokio::time::sleep(args.duration).await;
            handle.stop().await;
        }
    }
}

/// Stream the collector's reports (channel data) to the single
/// orchestrator connection, one framed report at a time.
async fn metrics_export(
    mut rx: mpsc::UnboundedReceiver<MetricReport>,
    listen: SocketAddr,
) {
    let listener = match TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("serve: metrics listen bind failed: {e}");
            return;
        }
    };
    eprintln!("serve: metrics listening at {listen} (one connection)");
    let (mut stream, _peer) = match listener.accept().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("serve: metrics accept failed (no orchestrator?): {e}");
            return;
        }
    };
    while let Some(report) = rx.recv().await {
        let frame = encode_report(&report);
        if stream.write_all(&frame).await.is_err() || stream.flush().await.is_err() {
            break; // orchestrator went away
        }
    }
}

/// Allocate a free loopback port (bind-port-0, take the number, close).
/// The race window is microseconds and the consumer (the spawned server)
/// binds immediately — fine for a load tool on loopback.
async fn alloc_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let p = l.local_addr().expect("local addr").port();
    drop(l);
    p
}

/// `taskset` on the PATH (item A's pinning primitive; `std` has no
/// affinity API and this crate forbids `unsafe`, so the syscall goes
/// through util-linux instead).
fn which_taskset() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("taskset");
            if cand.is_file() {
                return Some(cand);
            }
        }
        None
    })
}

/// Expand one `taskset` range element ("5" or "4-7").
fn range_cpus(r: &str) -> Option<Vec<u32>> {
    let mut it = r.split('-');
    let a: u32 = it.next()?.parse().ok()?;
    match it.next() {
        Some(b) => {
            let b: u32 = b.parse().ok()?;
            Some((a..=b).collect())
        }
        None => Some(vec![a]),
    }
}

/// Disjoint core sets from the CPU topology (`/sys/.../topology`): the
/// server gets the first `server_cores` *physical* cores (all their SMT
/// siblings), the client processes round-robin share the rest. Returns
/// `(server set, one set per client process)` as logical CPU numbers, or
/// `None` when the topology is not readable (non-Linux, or no SMT
/// information at all — in which case pinning is skipped rather than
/// guessing).
fn pin_masks(server_cores: u32, procs: u32) -> Option<(Vec<u32>, Vec<Vec<u32>>)> {
    let ncpu = std::thread::available_parallelism()
        .ok()?
        .get()
        .max(1) as u32;
    let mut groups: Vec<Vec<u32>> = Vec::new();
    for cpu in 0..ncpu {
        let path = format!(
            "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
        );
        let list = std::fs::read_to_string(&path).ok()?;
        let set: Vec<u32> = list
            .trim()
            .split(',')
            .filter_map(range_cpus)
            .flatten()
            .collect();
        if set.is_empty() || !set.contains(&cpu) {
            return None;
        }
        if !groups.iter().any(|g| g.as_slice() == set.as_slice()) {
            groups.push(set);
        }
    }
    if groups.is_empty() {
        return None;
    }
    let nserver = server_cores.min(groups.len() as u32) as usize;
    let mut server: Vec<u32> = groups[..nserver].iter().flatten().copied().collect();
    server.sort_unstable();
    let mut clients = vec![Vec::new(); procs as usize];
    for (i, g) in groups.iter().skip(nserver).enumerate() {
        clients[i % procs as usize].extend_from_slice(g);
    }
    for c in &mut clients {
        c.sort_unstable();
    }
    Some((server, clients))
}

/// Spawn a child process, optionally pinned to `mask` (logical CPUs) via
/// `taskset -c`. Stdout is piped only when `pipe_stdout` (the client
/// processes' CLIENT lines); stderr is always inherited (visible).
async fn spawn_pinned(
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
            let cpus = mask.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
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

/// Sum of utime+stime (clock ticks) from /proc/<pid>/stat. The comm
/// field may contain spaces/parens, so cut at the LAST ')': utime and
/// stime are then fields 12 and 13 of the remainder (1-based 14/15).
fn proc_ticks(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = s.rsplit(')').next()?.split_whitespace();
    let mut it = rest;
    let utime: u64 = it.nth(11)?.parse().ok()?; // field 14 (1-based)
    let stime: u64 = it.next()?.parse().ok()?; // field 15 (1-based)
    Some(utime + stime)
}

/// One child's stdout, line by line, into a channel (the reader task's
/// only awaited source: the pipe).
async fn read_lines(stdout: ChildStdout, tx: mpsc::UnboundedSender<String>) {
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
struct ClientRec {
    id: u64,
    connected: bool,
    connect_ms: u128,
    joined: bool,
    left: bool,
    snapshots: u64,
    bytes_in: u64,
    bytes_out: u64,
    moves: u64,
    errors: u64,
    join_rejected: u64,
    cap_rejected: u64,
    hz: Option<f64>,
}

fn parse_client_line(line: &str) -> Option<ClientRec> {
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
        errors: get("errors")?.parse().ok()?,
        join_rejected: get("join_rejected")?.parse().ok()?,
        cap_rejected: get("cap_rejected")?.parse().ok()?,
        hz: match get("hz")?.as_str() {
            "-" => None,
            v => v.parse().ok(),
        },
    })
}

/// The orchestrator (item A). Topology (all on loopback, all spawned by
/// this process):
///
/// ```text
/// orchestrator (this process)
///   ├─ server process   gsb-loadgen --serve --bind 127.0.0.1:P
///   │                   --metrics-listen 127.0.0.1:M
///   │      (its MetricReports arrive over the binary socket — the same
///   │      channel data the in-process mode drains; nothing parsed)
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
async fn orchestrate(args: Args) {
    init_tracing();
    let n = args.clients;
    let mut procs = args.procs.max(1);
    if (procs as u64) > n {
        procs = n as u32;
    }

    let server_port = alloc_port().await;
    let metrics_port = alloc_port().await;

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
        "orchestrate: N={n} procs={procs} visibility={} profile={} cell_size={} vision_radius={} max_snap_bytes={} spawn_half={} duration={}s move_ms={} stagger_ms={} pin={}",
        args.visibility,
        match args.profile {
            Profile::Ring => "ring",
            Profile::Spread => "spread",
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
    // The server's workers are sized to its pinned core set (or the
    // runtime default when unpinned); +3 s duration: the clean stop
    // happens after the clients left, so the final report windows cover
    // the leave flushes.
    let server_workers = masks
        .as_ref()
        .map(|m| m.0.len().max(1))
        .unwrap_or(args.workers.max(1));
    let mut sargs = vec![
        "--serve".into(),
        "--bind".into(),
        format!("127.0.0.1:{server_port}"),
        "--metrics-listen".into(),
        format!("127.0.0.1:{metrics_port}"),
        "--visibility".into(),
        args.visibility.to_string(),
        "--cell-size".into(),
        args.cell_size.to_string(),
        "--vision-radius".into(),
        args.vision_radius.to_string(),
        "--max-snapshot-bytes".into(),
        args.max_snapshot_bytes.to_string(),
        "--spawn-half-size".into(),
        args.server_spawn_half.to_string(),
        "--duration".into(),
        (args.duration + Duration::from_secs(3)).as_secs().to_string(),
        "--workers".into(),
        server_workers.to_string(),
    ];
    // Capacity / lifecycle guards (forwarded only when the operator
    // chose them; the served server keeps its config defaults otherwise).
    if let Some(n) = args.max_players {
        sargs.push("--max-players".into());
        sargs.push(n.to_string());
    }
    if let Some(n) = args.max_connections {
        sargs.push("--max-connections".into());
        sargs.push(n.to_string());
    }
    if let Some(s) = args.idle_timeout_secs {
        sargs.push("--idle-timeout-secs".into());
        sargs.push(s.to_string());
    }
    let mut server = spawn_pinned(
        &exe,
        &sargs,
        &[],
        &masks.as_ref().map(|m| m.0.clone()),
        taskset.as_deref(),
        "server",
        false,
    )
    .await
    .expect("spawn server child");
    let server_pid = server.id().expect("freshly spawned child has a pid");

    // Wait until the server's socket actually accepts, BEFORE spawning
    // the client children. A kernel-backlog handshake counts (the SYN
    // completes even before the accept loop wakes), so this only
    // de-races the process start-up — the clients never retry, and
    // their connect_ms stays a pure measurement of a ready server.
    // The probe itself becomes one clean open/close on the server (it
    // carries no frames and never joins a room).
    let server_addr: SocketAddr =
        format!("127.0.0.1:{server_port}").parse().expect("addr");
    let probe_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpStream::connect(server_addr).await {
            Ok(mut s) => {
                let _ = s.shutdown().await;
                break;
            }
            Err(_) if Instant::now() >= probe_deadline => {
                eprintln!("orchestrate: server socket not accepting after 10 s; clients will report their own connect failures");
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }

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
        let workers = masks
            .as_ref()
            .and_then(|m| m.1.get(p as usize))
            .map(|m| m.len().max(1))
            .unwrap_or(args.workers.max(1));
        let mut cargs = vec![
            count.to_string(),
            "--addr".into(),
            format!("127.0.0.1:{server_port}"),
            "--offset".into(),
            offset.to_string(),
            "--duration".into(),
            args.duration.as_secs().to_string(),
            "--move-ms".into(),
            args.move_ms.as_millis().to_string(),
            "--room".into(),
            args.room.to_string(),
            "--stagger-ms".into(),
            args.stagger_ms.to_string(),
            "--profile".into(),
            match args.profile {
                Profile::Ring => "ring".into(),
                Profile::Spread => "spread".into(),
            },
            "--spawn-half-size".into(),
            args.spawn_half.to_string(),
            "--workers".into(),
            workers.to_string(),
        ];
        // The flood client (by global id) belongs to exactly one child:
        // forward the flag only to the child whose id range contains it.
        if let Some(k) = args.flood_id
            && offset <= k
            && k < offset + count
        {
            cargs.push("--flood-id".into());
            cargs.push(k.to_string());
        }
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
    let metrics_addr: SocketAddr =
        format!("127.0.0.1:{metrics_port}").parse().expect("addr");
    let metrics_task = tokio::spawn(async move {
        // The server binds its metrics listener shortly after spawn;
        // retry until it accepts (one awaited source at a time).
        let mut stream = loop {
            match TcpStream::connect(metrics_addr).await {
                Ok(s) => break s,
                Err(e) => {
                    eprintln!("orchestrate: metrics connect {metrics_addr}: {e} (retrying)");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
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
        eprintln!("orchestrate: WARNING — a client child exited non-success; the merged numbers below cover what it reported");
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
    // channel closes on the clean stop), so this await is bounded.
    let server_reports: Vec<MetricReport> =
        metrics_task.await.expect("metrics reader panicked");

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
