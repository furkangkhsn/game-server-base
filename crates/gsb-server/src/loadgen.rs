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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};

use gsb_protocol::base::{
    Auth, Error, JoinRoom, JoinRoomResult, LeaveRoom, LeaveRoomResult,
};
use gsb_protocol::op;
use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use gsb_net::udp::UdpClient;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc;

use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::{
    fine_hist_percentile_us, hist_edge_us, MetricReport, NetReport, RegistryReport,
    RoomReport, FINE_HIST_BINS, FINE_HIST_CAP_US, HIST_BINS, HIST_OVERFLOW_BIN,
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
/// - [`Profile::Still`]: the *stillness* profile (the delta measurement):
///   the clients are split deterministically by id (the same id-based
///   determinism as the stagger) — the first `--still-frac` fraction is
///   "still": it issues ONE `MOVE_TO` (a ring target, the historical
///   profile's shape) and then stops sending entirely (its entity settles
///   at the target and the world stands still); the moving minority chases
///   the ring target as in `Ring`. The split is the id's hundredths digit
///   (1% granularity, deterministic per id like the stagger), so the ratio
///   holds for any client count. Both historical profiles move every
///   entity every tick, so a delta strategy shows no gain on them — the
///   stillness ratio is the axis on which the cell-encoded delta pays off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    Ring,
    Spread,
    Still,
}

impl Profile {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "ring" => Some(Self::Ring),
            "spread" => Some(Self::Spread),
            "still" => Some(Self::Still),
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
    /// The stillness ratio of the `still` profile (0..=1): the fraction of
    /// clients (deterministic per-id split) that settle once and stop
    /// moving; the rest chase the ring target (`--still-frac`, default 0.9).
    still_frac: f64,
    /// The map half-size for the `spread` profile's home distribution
    /// (must match the server's `spawn_half_size` so the spawn and the
    /// homes live on the same map).
    spawn_half: f32,
    /// The visibility strategy of the in-process / served server
    /// (`--visibility all|spatial|team|pvs|sharded`, default `all` — same
    /// as the server config default).
    visibility: gsb_server::Visibility,
    /// Shards per room (`--shard-count N`, default 4; used only for
    /// `sharded`). The map is a near-square grid of N shards; 1..=16.
    shard_count: u32,
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
    /// The transport the clients speak (and the in-process / served
    /// server listens on): `--transport tcp|udp` (default `tcp`). On
    /// `udp` the client's `connect_ms` is the rUDP cookie-HANDSHAKE
    /// latency (challenge + proof), not a TCP handshake.
    transport: gsb_server::TransportKind,
    /// TLS client material (`--tls-ca`, docs/SECURITY.md §2 decision 4):
    /// when set, TCP clients wrap their socket in a rustls handshake that
    /// verifies the server against THIS root. External mode only — the
    /// in-process server stays plaintext this round (there is no flag to
    /// mint or load server keys here). `connect_ms` then measures the TLS
    /// handshake too.
    tls_ca: Option<String>,
    /// The name the server certificate must carry (`--tls-server-name`,
    /// default "localhost" — what the test PKI mints).
    tls_server_name: String,
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
    /// Churn-cycle length in seconds (`--churn-secs S`, RECONNECT §14.5):
    /// when set, every client cycles connect → auth+join → MOVE briefly →
    /// DROP the socket WITHOUT a leave → reconnect with the SAME identity
    /// every S seconds instead of one plain session. The reconnect hits
    /// the parked entity (its transport died inside the park grace, so
    /// the room held it) and comes back with the SAME wire id — the
    /// mass-resume storm measured against the real server.
    churn_secs: Option<f64>,
    /// How many DROP→resume transitions each churn client performs
    /// before it stops dropping and simply keeps playing
    /// (`--churn-cycles K`; 0 = until the deadline). One transition per
    /// identity is the exact shape of the §14.5 thundering herd: N
    /// simultaneous resumes against the server, nothing else in flight.
    churn_cycles: u64,
    /// The served/in-process server's disconnect-park grace
    /// (`--disconnect-grace-secs F`; RECONNECT §3). Unspecified = the
    /// server config default (30 s). A plain run never needs this; a
    /// churn run wants it comfortably LONGER than the churn cycle so
    /// every reconnect lands INSIDE the hold (pure resume, no expiry),
    /// which is exactly the thundering-herd shape §14.5 measures.
    disconnect_grace_secs: Option<f64>,
}

/// The usage text (`--help` / `-h`).
const USAGE: &str = "\
gsb-loadgen — load generator for gsb servers (N real-socket clients)

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
  --transport tcp|udp       client transport (default tcp). udp = rUDP:
                            stateless cookie handshake, reliable control
                            band, loss-tolerant snapshot band; connect_ms
                            then measures the handshake
  --profile ring|spread|still movement profile (default ring — the historical
                            clustered layout; spread = uniform over the
                            ±spawn-half map, the sparse MOBA-like layout;
                            still = a configurable stillness ratio: the still
                            clients settle once, the minority moves)
  --still-frac F            fraction of still clients for the still profile
                            (default 0.9; deterministic per-id split)
  --spawn-half-size F       map half-size for the spread profile's homes
                            and the (in-process/served) server's spawn
                            points (default: 50 for ring, 1000 for spread)
  --workers N               tokio worker threads for this process

Server options (in-process server, --serve, or the orchestrator's server):
  --visibility all|spatial|team|pvs|sharded   (default all)
  --shard-count N                     (sharded; near-square grid of N
                                       shards, 1..=16; default 4 = 2×2)
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
        still_frac: 0.9,
        spawn_half: 50.0,
        visibility: gsb_server::Visibility::default(),
        shard_count: 4,
        cell_size: 20.0,
        vision_radius: gsb_game::team::DEFAULT_VISION_RADIUS,
        max_snapshot_bytes: 1400,
        server_spawn_half: 50.0,
        serve: false,
        bind: "127.0.0.1:7777".into(),
        metrics_listen: None,
        orchestrate: false,
        transport: gsb_server::TransportKind::Tcp,
        tls_ca: None,
        tls_server_name: "localhost".into(),
        procs: 1,
        pin: false,
        pin_server_cores: 8,
        workers: 0,
        max_players: None,
        max_connections: None,
        idle_timeout_secs: None,
        flood_id: None,
        churn_secs: None,
        churn_cycles: 0,
        disconnect_grace_secs: None,
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
                    .unwrap_or_else(|| panic!("--profile: expected ring|spread|still (try --help)"))
            }
            "--still-frac" => {
                let f: f64 = v().parse().expect("number");
                assert!((0.0..=1.0).contains(&f), "--still-frac must be in 0..=1");
                args.still_frac = f;
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
                    "sharded" => gsb_server::Visibility::Sharded,
                    other => panic!(
                        "--visibility: expected all|spatial|team|pvs|sharded, got {other}"
                    ),
                };
            }
            "--shard-count" => {
                args.shard_count = v().parse().expect("number");
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
            "--churn-secs" => {
                let f: f64 = v().parse().expect("number");
                assert!(f > 0.0, "--churn-secs must be > 0");
                args.churn_secs = Some(f);
            }
            "--disconnect-grace-secs" => {
                args.disconnect_grace_secs = Some(v().parse().expect("number"));
            }
            "--churn-cycles" => {
                args.churn_cycles = v().parse().expect("number");
            }
            "--transport" => {
                let s = v();
                args.transport = match s.as_str() {
                    "tcp" => gsb_server::TransportKind::Tcp,
                    "udp" => gsb_server::TransportKind::Udp,
                    other => panic!("--transport: expected tcp|udp, got {other}"),
                };
            }
            "--tls-ca" => args.tls_ca = Some(v()),
            "--tls-server-name" => args.tls_server_name = v(),
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
    // TLS is an external-client feature this round: there is no way to hand
    // the in-process/served server its cert/key here, so a CA without an
    // external target would silently test plaintext against a plaintext
    // server — refuse instead of lying. And rUDP takes no TLS anywhere
    // (docs/SECURITY.md §2 decision 7).
    if args.tls_ca.is_some() && args.addr.is_none() {
        panic!(
            "--tls-ca requires --addr HOST:PORT: the in-process server has no \
             TLS config this round (it stays plaintext)"
        );
    }
    if args.tls_ca.is_some() && args.transport == gsb_server::TransportKind::Udp {
        panic!("--tls-ca with --transport udp is contradictory: rUDP takes no TLS");
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

async fn read_frame(r: &mut (dyn AsyncRead + Unpin + Send)) -> Option<(u16, Vec<u8>)> {
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
    /// Protocol-violation-budget closes observed (`ERROR` code 9 whose
    /// message names the violation budget): the anti-amplification
    /// guardrail closing a connection that exceeded its budget. Same
    /// *code* as the capacity close (both are "server closed the
    /// connection"); the message string is what separates them.
    budget_rejected: u64,
    /// rUDP transport statistics (all zero on TCP): the client's own
    /// reliable retransmissions, duplicated inbound REL frames (the
    /// server's retransmissions), inbound drops on a full out-of-order
    /// window, and outbound control frames given up (no ACK in time).
    retrans_out: u64,
    dup_in: u64,
    oob_dropped: u64,
    gave_up: u64,
    /// First/last snapshot sequence with its arrival instant: the server's
    /// measured tick rate is (last_seq − first_seq) / Δt, since the
    /// snapshot sequence is the global tick index.
    seq_first: Option<(u64, Instant)>,
    seq_last: Option<(u64, Instant)>,
    /// Input acknowledgments received (Section A; the server's per-
    /// connection high-water marks).
    acks: u64,
    /// The highest `processed_up_to` observed over all acks.
    ack_processed_max: u64,
    /// The worst ack lag in ms (send instant of the acked seq → ack
    /// arrival; 0 when no numbered input was acked).
    ack_lag_max_ms: u128,
    /// Full snapshots applied to the client view (group frames with
    /// `delta = false`, whatever their source: a fresh group's first
    /// packet, a keep-alive full, or a one-shot private full).
    fulls: u64,
    /// One-shot private fulls received (`Private{snapshot}` — the late-
    /// join / group-crossing baseline; a trigger-frequency measurement).
    private_fulls: u64,
    /// Delta snapshots applied (`delta = true`).
    deltas: u64,
    /// Deltas dropped (no baseline, or a sequence gap — a lost snapshot
    /// before them; the loss-recovery counter, healed by the next full).
    gap_drops: u64,
    /// Entities in the client view at the end of the run.
    view_size: u64,
    /// Churn mode only (RECONNECT §14.5): completed connect→drop cycles.
    churn_cycles: u64,
    /// Churn mode only: joins that came back onto the SAME wire id (a
    /// server-accepted resume — the counter the profile exists to move).
    resumed: u64,
    /// Churn mode only: joins that got a DIFFERENT wire id than the
    /// previous session (the park was already gone — expiry/supersede —
    /// and the client transparently fresh-joined, §5).
    fresh_joins: u64,
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

/// The client-side TLS material (a cloned slice of `Args`): the CA root to
/// trust and the name to expect in the server certificate. `None` =
/// plaintext TCP.
#[derive(Clone)]
struct TlsOpts {
    ca_path: String,
    server_name: String,
}

/// Everything one client task needs besides its own id. (One struct
/// rather than eight scalars — the profile work kept adding fields.)
#[derive(Clone)]
struct ClientParams {
    /// TLS material for TCP clients (`None` = plaintext, the default).
    tls: Option<TlsOpts>,
    addr: SocketAddr,
    room: u64,
    move_ms: Duration,
    stagger_ms: f64,
    profile: Profile,
    still_frac: f64,
    spawn_half: f32,
    /// The AOI cell size (for the client view's `CellExit` handling;
    /// ignored by the non-spatial strategies, whose snapshots are full).
    cell_size: f32,
    deadline: Instant,
    /// Flood mode (the `--flood-id` client): after joining, write MOVE_TO
    /// in a tight loop until the deadline — the input-flood behaviour
    /// probe for the per-connection pull budget and the drop attribution.
    flood: bool,
    /// The client's transport (TCP or rUDP; see the `Wire` below).
    kind: gsb_server::TransportKind,
}

/// Build a rustls connector trusting ONLY the CA PEM at `ca_path` (the
/// `--tls-ca` root; a self-signed test CA works — docs/SECURITY.md §2).
fn tls_connector(ca_path: &str) -> tokio_rustls::TlsConnector {
    let pem = std::fs::read_to_string(ca_path)
        .unwrap_or_else(|e| panic!("cannot read --tls-ca `{ca_path}`: {e}"));
    let certs = rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| panic!("malformed certificate PEM in `{ca_path}`: {e}"));
    let mut roots = rustls::RootCertStore::empty();
    for c in certs {
        roots.add(c).expect("--tls-ca PEM is not a certificate");
    }
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
}

/// Connect one wire of the given kind (the transport-specific half of a
/// client session's birth; shared by the plain and the churn client —
/// TCP gets `nodelay` + a split into boxed halves so plaintext and TLS
/// share one wire shape, rUDP runs its cookie handshake). With TLS
/// material, `connect_ms` includes the rustls handshake.
async fn connect_wire(
    kind: gsb_server::TransportKind,
    addr: SocketAddr,
    tls: &Option<TlsOpts>,
) -> std::io::Result<Wire> {
    Ok(match kind {
        gsb_server::TransportKind::Udp => Wire::Udp(Box::new(UdpClient::connect(addr).await?)),
        gsb_server::TransportKind::Tcp => match tls {
            None => {
                let stream = TcpStream::connect(addr).await?;
                stream.set_nodelay(true).ok();
                let (r, w) = tokio::io::split(stream);
                Wire::Tcp {
                    r: Box::new(r),
                    w: Box::new(w),
                }
            }
            Some(opts) => {
                let stream = TcpStream::connect(addr).await?;
                stream.set_nodelay(true).ok();
                let connector = tls_connector(&opts.ca_path);
                let dns: rustls::pki_types::ServerName<'static> = opts
                    .server_name
                    .clone()
                    .try_into()
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("--tls-server-name `{}` is not a DNS name", opts.server_name),
                        )
                    })?;
                // The handshake happens HERE: connect_ms covers it (the
                // same convention as the rUDP cookie handshake above).
                let tls_stream = connector.connect(dns, stream).await?;
                let (r, w) = tokio::io::split(tls_stream);
                Wire::Tcp {
                    r: Box::new(r),
                    w: Box::new(w),
                }
            }
        },
    })
}

/// What one bounded receive on the wire found. TCP distinguishes death
/// (EOF) from quiet; rUDP has no EOF, so "quiet" is all it can report —
/// the deadline ends those runs.
enum Got {
    Frame(u16, Vec<u8>),
    Quiet,
    Dead,
}

async fn recv_wire(wire: &mut Wire, timeout: Duration) -> Got {
    match wire {
        Wire::Tcp { r, .. } => {
            match tokio::time::timeout(timeout, read_frame(r.as_mut())).await {
                Ok(Some((op, payload))) => Got::Frame(op, payload),
                Ok(None) => Got::Dead, // EOF / bad frame
                Err(_) => Got::Quiet,
            }
        }
        Wire::Udp(c) => match c.recv_frame(timeout).await {
            Ok(Some(f)) => Got::Frame(f.op, f.payload.to_vec()),
            _ => Got::Quiet,
        },
    }
}

async fn send_wire(wire: &mut Wire, op: u16, payload: Vec<u8>) -> std::io::Result<()> {
    match wire {
        Wire::Tcp { w, .. } => {
            let f = frame(op, &payload);
            w.write_all(&f).await?;
            w.flush().await
        }
        Wire::Udp(c) => c.send_frame(op, payload).await,
    }
}

/// The wire to the server (the only place TCP and rUDP diverge inside
/// the client loop — see `run_client`).
enum Wire {
    /// Length-prefixed frames over a per-connection socket, split: the
    /// main loop owns the read half, the flood path the write half.
    /// Type-erased halves so plaintext TCP and TLS-over-TCP share this
    /// one variant (the framing below cannot tell them apart).
    Tcp {
        r: Box<dyn AsyncRead + Unpin + Send>,
        w: Box<dyn AsyncWrite + Unpin + Send>,
    },
    /// One shared socket in one task: read and write interleave (UDP has
    /// no connection to split). Boxed: `UdpClient` carries a 2 KB read
    /// buffer + queues (keeps the enum small — clippy's
    /// `large_enum_variant`).
    Udp(Box<UdpClient>),
}

/// The rUDP datagram size of one frame (client-side byte accounting
/// mirrors the bytes actually sent: RAW = kind + op + payload; REL =
/// kind + seq + op + payload).
fn wire_in_bytes(op: u16, payload_len: usize) -> u64 {
    let header = if (1..=64).contains(&op) && op != op::base::UDP_ACK {
        5
    } else {
        1
    };
    (header + 2 + payload_len) as u64
}

/// The client-side world view the protocol prescribes (see the
/// `WorldSnapshot` docs in `gsb_game/proto/game.proto`): a full REPLACES
/// the view; a delta applies ON TOP in the fixed order `removed` →
/// `cell_exits` → `entities` — even across a sequence gap (the stream is
/// event-driven: a gap is normal, and the records are absolute, so a
/// stale view is the worst case; the keep-alive full is the convergence
/// guarantee); a delta with NO baseline at all (a fresh client) is
/// DROPPED until the next full; a duplicate/stale sequence (<= the last
/// accepted) is discarded. The cell of a stored position uses the server's own formula
/// (floor of the WIRE coordinates / cell_size), so a `CellExit` record
/// forgets exactly the entities the server considers to be in that cell.
struct ClientView {
    entities: HashMap<u64, (i32, i32)>,
    last_seq: Option<u64>,
    cell_size: f32,
}

/// The outcome of applying one snapshot (the report's counters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Apply {
    /// A full was applied (the view was replaced).
    Full,
    /// A delta was applied on top (consecutive or across a gap — the
    /// stream is event-driven, so a gap is normal, not loss; the
    /// keep-alive full is the convergence guarantee).
    Delta,
    /// A delta dropped with no baseline at all (a fresh client before
    /// its first full — healed by the one-shot private full / the next
    /// keep-alive full).
    NoBaseline,
    /// A duplicate/stale sequence: discarded (not an error).
    Stale,
}

impl ClientView {
    #[inline]
    fn cell_of(x: i32, y: i32, cell_size: f32) -> (i32, i32) {
        ((x as f32 / cell_size).floor() as i32, (y as f32 / cell_size).floor() as i32)
    }

    fn apply(&mut self, s: &gsb_game::game::WorldSnapshot) -> Apply {
        if s.sequence <= self.last_seq.unwrap_or(0) {
            return Apply::Stale;
        }
        if s.delta {
            // A delta needs a baseline (a full applied before it); a
            // sequence gap does NOT disqualify it (event-driven stream —
            // see the message docs in `game.proto`).
            if self.last_seq.is_none() {
                return Apply::NoBaseline;
            }
            for &w in &s.removed {
                self.entities.remove(&w);
            }
            for c in &s.cell_exits {
                let cell = Self::cell_of(c.x, c.y, self.cell_size);
                self.entities
                    .retain(|_, (x, y)| Self::cell_of(*x, *y, self.cell_size) != cell);
            }
            for e in &s.entities {
                self.entities.insert(e.entity, (e.x, e.y));
            }
            self.last_seq = Some(s.sequence);
            return Apply::Delta;
        }
        self.entities.clear();
        for e in &s.entities {
            self.entities.insert(e.entity, (e.x, e.y));
        }
        self.last_seq = Some(s.sequence);
        Apply::Full
    }

    /// The one-shot private full (a per-connection baseline reset — see
    /// the `Private{snapshot}` docs in `game.proto`): applied
    /// UNCONDITIONALLY, outside the group stream's sequence logic.
    fn apply_private_full(&mut self, s: &gsb_game::game::WorldSnapshot) {
        self.entities.clear();
        for e in &s.entities {
            self.entities.insert(e.entity, (e.x, e.y));
        }
        self.last_seq = Some(s.sequence);
    }
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
        budget_rejected: 0,
        retrans_out: 0,
        dup_in: 0,
        oob_dropped: 0,
        gave_up: 0,
        seq_first: None,
        seq_last: None,
        acks: 0,
        ack_processed_max: 0,
        ack_lag_max_ms: 0,
        fulls: 0,
        private_fulls: 0,
        deltas: 0,
        gap_drops: 0,
        view_size: 0,
        churn_cycles: 0,
        resumed: 0,
        fresh_joins: 0,
    };

    // The client-side world view (the delta protocol's client half — see
    // `ClientView`; fulls replace, deltas apply on top, gaps drop until
    // the next full).
    let mut view = ClientView {
        entities: HashMap::new(),
        last_seq: None,
        cell_size: p.cell_size,
    };

    // Optional connect stagger (see `Args::stagger_ms`).
    if p.stagger_ms > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(id as f64 * p.stagger_ms / 1000.0)).await;
    }
    // The wire to the server: the ONLY place the two transports diverge
    // inside the client loop (everything above and below it is
    // transport-agnostic). On rUDP `connect` is the cookie handshake, so
    // `connect_ms` measures the handshake latency.
    let t0 = Instant::now();
    let mut wire = match connect_wire(p.kind, p.addr, &p.tls).await {
        Ok(w) => w,
        Err(e) => {
            eprintln!("client {id}: connect failed: {e}");
            return rep;
        }
    };
    rep.connect_ms = t0.elapsed().as_millis();
    rep.connected = true;

    // AUTH + JOIN (one coalesced write on TCP — the connection actor
    // drains in order; two frames on rUDP — its reliable control band
    // orders them).
    let auth_payload = Auth {
        name: format!("lg-{id}"),
        ticket: vec![],
    }
    .encode_to_vec();
    let join_payload = JoinRoom { room_id: p.room }.encode_to_vec();
    match &mut wire {
        Wire::Tcp { w, .. } => {
            let mut out = frame(op::base::AUTH_REQ, &auth_payload);
            out.extend(frame(op::base::JOIN_ROOM_REQ, &join_payload));
            rep.bytes_out += out.len() as u64;
            if w.write_all(&out).await.is_err() || w.flush().await.is_err() {
                return rep;
            }
        }
        Wire::Udp(c) => {
            rep.bytes_out += wire_in_bytes(op::base::AUTH_REQ, auth_payload.len());
            if c.send_frame(op::base::AUTH_REQ, auth_payload).await.is_err() {
                return rep;
            }
            rep.bytes_out += wire_in_bytes(op::base::JOIN_ROOM_REQ, join_payload.len());
            if c.send_frame(op::base::JOIN_ROOM_REQ, join_payload).await.is_err() {
                return rep;
            }
        }
    }

    let t_start = Instant::now();
    let mut last_move = t_start;
    let mut flooded = false;
    // Section A: inputs are NUMBERED (monotonic from 1 per session); the
    // server acks its per-connection high-water mark in the Private frame.
    // `sent_at` keeps the send instant of seq N at index N-1 (for the
    // ack-lag measurement) — one entry per numbered input, a few dozen
    // per client per run.
    let mut next_seq: u64 = 1;
    let mut sent_at: Vec<Instant> = Vec::new();
    // The still profile's deterministic per-id split (the same id-based
    // determinism as the stagger, 1% granularity — the id's hundredths
    // digit decides, so any client count gets the ratio): the first
    // `still_frac` fraction of the ids is "still" — it issues ONE MOVE_TO
    // (settling at a ring target) and then sends nothing more; the moving
    // minority chases the ring target as in the historical profile.
    let is_still =
        p.profile == Profile::Still && (id % 100) as f64 / 100.0 < p.still_frac;
    let mut settled = false;
    loop {
        let now = Instant::now();
        if now >= p.deadline {
            break;
        }
        if now.duration_since(last_move) >= p.move_ms {
            last_move = now;
            // A still client settles once: the first interval sends its
            // only command, the rest of the run it is silent (that IS the
            // profile — the majority of the world stands still).
            if !(is_still && settled) {
                settled = true;
                let (tx, ty) = match p.profile {
                    // The historical profile (UNCHANGED — all previous
                    // measurements stay comparable): a circle of radius 40
                    // around the map center at 4 rad/s, phase-shifted per
                    // client (id-based offset so N entities do not move in
                    // lockstep). The targets outrun the entities, so the
                    // entities crowd the central band — the *clustered*
                    // layout.
                    Profile::Ring => {
                        let angle = (now.duration_since(t_start).as_secs_f64() + id as f64 * 0.618)
                            * 4.0;
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
                        let w =
                            (now.duration_since(t_start).as_secs_f64() + id as f64 * 0.618) * 0.4;
                        (hx + w.cos() * 20.0, hy + w.sin() * 20.0)
                    }
                    // The still profile's moving minority chases the ring
                    // target (the historical profile's shape).
                    Profile::Still => {
                        let angle =
                            (now.duration_since(t_start).as_secs_f64() + id as f64 * 0.618) * 4.0;
                        (angle.cos() * 40.0, angle.sin() * 40.0)
                    }
                };
                let seq = next_seq;
                next_seq += 1;
                sent_at.push(now);
                let msg = gsb_game::game::MoveTo {
                    x: tx as i32,
                    y: ty as i32,
                    seq,
                };
                let move_payload = msg.encode_to_vec();
                rep.moves += 1;
                match &mut wire {
                    Wire::Tcp { w, .. } => {
                        let f = frame(gsb_game::op::MOVE_TO, &move_payload);
                        rep.bytes_out += f.len() as u64;
                        if w.write_all(&f).await.is_err() || w.flush().await.is_err() {
                            break; // peer gone
                        }
                    }
                    Wire::Udp(c) => {
                        rep.bytes_out +=
                            wire_in_bytes(gsb_game::op::MOVE_TO, move_payload.len());
                        if c.send_frame(gsb_game::op::MOVE_TO, move_payload).await.is_err() {
                            break; // session gone (the writer gave up)
                        }
                    }
                }
            }
        }
        let timeout = p.deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(250));
        // TCP: None from read_frame = EOF (the loop breaks below); rUDP:
        // None = "quiet window" (no EOF exists — the deadline ends the
        // run instead).
        let got = match &mut wire {
            Wire::Tcp { r, .. } => {
                tokio::time::timeout(timeout, read_frame(r.as_mut())).await.ok().flatten()
            }
            Wire::Udp(c) => c
                .recv_frame(timeout)
                .await
                .ok()
                .flatten()
                .map(|f| (f.op, f.payload.to_vec())),
        };
        let Some((op, payload)) = got else {
            continue; // timeout: loop
        };
        rep.bytes_in += match &wire {
            Wire::Tcp { .. } => (4 + 2 + payload.len()) as u64,
            Wire::Udp(_) => wire_in_bytes(op, payload.len()),
        };
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
                    // The client half of the delta protocol (see
                    // `ClientView`): apply it, whatever the strategy's
                    // mode (a full-snapshot room's frames are all fulls).
                    match view.apply(&m) {
                        Apply::Full => rep.fulls += 1,
                        Apply::Delta => rep.deltas += 1,
                        Apply::NoBaseline => rep.gap_drops += 1,
                        Apply::Stale => {}
                    }
                }
                Err(_) => rep.errors += 1,
            },
            gsb_game::op::PRIVATE => {
                let pr = match gsb_game::game::Private::decode(&payload[..]) {
                    Ok(pr) => pr,
                    Err(_) => {
                        rep.errors += 1;
                        continue;
                    }
                };
                match pr.payload {
                    Some(gsb_game::game::private::Payload::Ack(ack)) => {
                        // Section A: the server's per-connection input
                        // high-water mark. `now` is this loop iteration's
                        // instant — the ack's lag is measured against the
                        // send instant of the acked seq (index seq-1).
                        rep.acks += 1;
                        rep.ack_processed_max = rep.ack_processed_max.max(ack.processed_up_to);
                        if ack.processed_up_to > 0 {
                            let i = ack.processed_up_to as usize - 1;
                            if i < sent_at.len() {
                                let lag = Instant::now().duration_since(sent_at[i]);
                                rep.ack_lag_max_ms = rep.ack_lag_max_ms.max(lag.as_millis());
                            }
                        }
                    }
                    Some(gsb_game::game::private::Payload::Snapshot(sn)) => {
                        // A one-shot FULL view (a fresh group member — late
                        // join or a group crossing). It MUST be a full: a
                        // delta here would be a protocol error, and a
                        // wrong-mode client must not silently misapply it.
                        if sn.delta {
                            rep.errors += 1;
                        } else {
                            rep.private_fulls += 1;
                            rep.fulls += 1;
                            view.apply_private_full(&sn);
                        }
                    }
                    None => rep.errors += 1,
                }
            }
            op::base::ERROR => {
                let e: Error = Error::decode(&payload[..]).unwrap_or_else(|_| Error::default());
                match e.code {
                    // The guardrails, observed from the client side:
                    // 8 = room full (gentle reject, connection stays);
                    // 9 = server closed the connection — either the
                    // connection-capacity cap or the protocol-violation
                    // budget (same code; the message separates them).
                    8 => rep.join_rejected += 1,
                    9 if e.message.contains("violation") => rep.budget_rejected += 1,
                    9 => rep.cap_rejected += 1,
                    _ => rep.errors += 1,
                }
            }
            _ => {}
        }
    }

    // The final client view size (the delta protocol's end state).
    rep.view_size = view.entities.len() as u64;

    if flooded {
        // The input flood: write MOVE_TO as fast as the socket accepts,
        // until the deadline. The server-side chain (reader pump → conn
        // inbox → conn actor → action channel → room pull budget) bounds
        // what actually reaches the tick; the excess is dropped on the
        // flooder's OWN full action channel (attributed to it). The flood
        // stays UNNUMBERED (seq 0, legacy): it probes the drop-attribution
        // guardrails, not the sequence rule (a numbered flood would only
        // spin the high-water mark).
        let msg = gsb_game::game::MoveTo { x: 0, y: 0, seq: 0 };
        match &mut wire {
            Wire::Tcp { w, .. } => {
                let f = frame(gsb_game::op::MOVE_TO, &msg.encode_to_vec());
                while Instant::now() < p.deadline {
                    if w.write_all(&f).await.is_err() {
                        break; // peer gone
                    }
                    rep.moves += 1;
                    rep.bytes_out += f.len() as u64;
                }
            }
            Wire::Udp(c) => {
                // rUDP: the client is ONE task (read and write share the
                // socket), so the flood interleaves NON-BLOCKING
                // read-drains; the flood frames travel the lossy game
                // band, so retransmit state never gets in the way.
                let payload = msg.encode_to_vec();
                while Instant::now() < p.deadline {
                    if c.send_frame(gsb_game::op::MOVE_TO, payload.clone())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    rep.moves += 1;
                    rep.bytes_out += wire_in_bytes(gsb_game::op::MOVE_TO, payload.len());
                    while c.recv_frame(Duration::ZERO).await.ok().flatten().is_some() {}
                }
            }
        }
    }

    // Graceful leave (counted by the server's join/leave metrics) and
    // wait for the ack: without it, the socket close — and any server
    // shutdown that follows — can race ahead of the leave, and the
    // server never counts it. (rUDP: the leave is a control-band frame,
    // so it is retransmitted until the server ACKs it; there is no EOF
    // to race — the 500 ms window ends the wait.)
    let leave_payload = LeaveRoom {}.encode_to_vec();
    let leave_sent = match &mut wire {
        Wire::Tcp { w, .. } => {
            let f = frame(op::base::LEAVE_ROOM_REQ, &leave_payload);
            rep.bytes_out += f.len() as u64;
            w.write_all(&f).await.is_ok() && w.flush().await.is_ok()
        }
        Wire::Udp(c) => {
            rep.bytes_out += wire_in_bytes(op::base::LEAVE_ROOM_REQ, leave_payload.len());
            c.send_frame(op::base::LEAVE_ROOM_REQ, leave_payload).await.is_ok()
        }
    };
    if leave_sent {
        let leave_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < leave_deadline {
            let timeout = leave_deadline.saturating_duration_since(Instant::now());
            let got = match &mut wire {
                Wire::Tcp { r, .. } => {
                    tokio::time::timeout(timeout, read_frame(r.as_mut())).await.ok().flatten()
                }
                Wire::Udp(c) => c
                    .recv_frame(timeout)
                    .await
                    .ok()
                    .flatten()
                    .map(|f| (f.op, f.payload.to_vec())),
            };
            let Some((op, payload)) = got else {
                break;
            };
            rep.bytes_in += match &wire {
                Wire::Tcp { .. } => (4 + 2 + payload.len()) as u64,
                Wire::Udp(_) => wire_in_bytes(op, payload.len()),
            };
            if op == op::base::LEAVE_ROOM_RESULT {
                let _ = LeaveRoomResult::decode(&payload[..]);
                rep.left = true;
                break;
            }
        }
    }
    // The client's rUDP transport statistics (all zero on TCP).
    if let Wire::Udp(c) = &wire {
        rep.retrans_out = c.stats.retrans_out;
        rep.dup_in = c.stats.dup_in;
        rep.oob_dropped = c.stats.oob_dropped;
        rep.gave_up = c.stats.gave_up;
    }
    rep
}

// ════════════════════════════════════════════════════════════════════════
// Churn profile (RECONNECT §14.5 — the mass-reconnect storm): N clients
// each cycle connect → auth+join → a few MOVE_TOs → DROP THE SOCKET
// WITHOUT A LEAVE → sleep out the rest of the cycle → reconnect with the
// SAME identity. Every reconnect after the first lands on a PARKED
// entity (the drop happened inside the disconnect grace), so the join
// comes back with the SAME wire id: a server-accepted resume. This is
// the user-visible reconnect feature exercised at load scale against
// the real transport.
// ════════════════════════════════════════════════════════════════════════

/// One auth+join round trip of a churn session, RETRYING the join on a
/// gentle rejection (ERROR code 4 — the stale-resume answer a fresh
/// connection can elicit after an earlier session already rebound the
/// park; the core's per-connection join-epoch counter grows only within
/// ONE connection, so the retry's higher epoch is accepted and the SAME
/// entity comes back). A real game client retries a transient join
/// failure; so do we, with a bounded budget.
async fn churn_join(
    id: u64,
    wire: &mut Wire,
    room: u64,
    rep: &mut ClientReport,
) -> Option<u64> {
    for _attempt in 0..5u32 {
        let join_payload = JoinRoom { room_id: room }.encode_to_vec();
        if send_wire(wire, op::base::JOIN_ROOM_REQ, join_payload)
            .await
            .is_err()
        {
            return None;
        }
        let mut retriable = false;
        let join_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < join_deadline {
            match recv_wire(wire, Duration::from_millis(250)).await {
                Got::Frame(op::base::JOIN_ROOM_RESULT, payload) => {
                    return JoinRoomResult::decode(&payload[..])
                        .ok()
                        .map(|m| m.entity);
                }
                Got::Frame(op::base::ERROR, payload) => {
                    let e = Error::decode(&payload[..]).unwrap_or_default();
                    match e.code {
                        // The gentle stale-resume reject: retry with the
                        // same connection's next epoch.
                        4 => {
                            if !retriable {
                                eprintln!(
                                    "churn client {id}: join answered 'stale                                      resume' (code 4); retrying on the same                                      connection (core join-epoch quirk)"
                                );
                            }
                            retriable = true;
                            break;
                        }
                        8 => rep.join_rejected += 1,
                        9 => rep.cap_rejected += 1,
                        _ => rep.errors += 1,
                    }
                }
                Got::Frame(_, payload) => rep.bytes_in += payload.len() as u64,
                Got::Quiet => {}
                Got::Dead => return None,
            }
        }
        if !retriable {
            return None;
        }
    }
    None
}

async fn run_churn_client(id: u64, p: ClientParams, cycle: Duration, max_drops: u64) -> ClientReport {
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
        budget_rejected: 0,
        retrans_out: 0,
        dup_in: 0,
        oob_dropped: 0,
        gave_up: 0,
        seq_first: None,
        seq_last: None,
        acks: 0,
        ack_processed_max: 0,
        ack_lag_max_ms: 0,
        fulls: 0,
        private_fulls: 0,
        deltas: 0,
        gap_drops: 0,
        view_size: 0,
        churn_cycles: 0,
        resumed: 0,
        fresh_joins: 0,
    };
    // ONE identity for every session of this client (the resume key):
    // this is what makes the reconnects RESUMES instead of fresh joins.
    let name = format!("lg-{id}");
    // The wire id of the previous session (0 before the first join): the
    // continuity check that classifies each join as resume / fresh.
    let mut prev_entity: u64 = 0;
    // Completed DROP transitions so far (`max_drops` reached ⇒ the last
    // session lingers instead of dropping).
    let mut drops: u64 = 0;
    while Instant::now() < p.deadline {
        let cycle_end = (Instant::now() + cycle).min(p.deadline);
        let final_session = max_drops != 0 && drops >= max_drops;
        rep.churn_cycles += 1;

        // -- connect ───────────────────────────────────────────────────
        let t0 = Instant::now();
        let mut wire = match connect_wire(p.kind, p.addr, &p.tls).await {
            Ok(w) => w,
            Err(e) => {
                eprintln!("churn client {id}: connect failed: {e}");
                rep.errors += 1;
                tokio::time::sleep(cycle_end.saturating_duration_since(Instant::now())).await;
                continue;
            }
        };
        rep.connected = true;
        rep.connect_ms = t0.elapsed().as_millis();

        // -- auth + join (coalesced write on TCP; two frames on rUDP) ──
        let auth_payload = Auth {
            name: name.clone(),
            ticket: vec![],
        }
        .encode_to_vec();
        if send_wire(&mut wire, op::base::AUTH_REQ, auth_payload)
            .await
            .is_err()
        {
            continue;
        }
        rep.bytes_out += wire_in_bytes(op::base::AUTH_REQ, 8);

        // -- wait for JOIN_ROOM_RESULT (bounded, with bounded retry) ───
        let Some(entity) = churn_join(id, &mut wire, p.room, &mut rep).await else {
            // Never joined this cycle: nothing to park; just end it.
            drop(wire);
            tokio::time::sleep(cycle_end.saturating_duration_since(Instant::now())).await;
            continue;
        };
        rep.joined = true;
        if prev_entity == 0 {
            // First session: a plain join by definition.
        } else if entity == prev_entity {
            rep.resumed += 1; // SAME wire id: the park was consumed by a resume
        } else {
            rep.fresh_joins += 1; // different id: the hold had already ended (§5 fallback)
        }
        prev_entity = entity;

        // -- move until shortly before the cycle boundary, draining the
        //    inbound stream (the same interleaved shape as run_client) ─
        const DROP_MARGIN: Duration = Duration::from_millis(100);
        let phase_end = if final_session { p.deadline } else { cycle_end };
        let mut next_move = Instant::now();
        let mut seq: u64 = 1; // every session numbers inputs from 1 (§14.2 reset)
        loop {
            let now = Instant::now();
            if now + DROP_MARGIN >= phase_end {
                break;
            }
            if now >= next_move {
                next_move = now + p.move_ms;
                let msg = gsb_game::game::MoveTo {
                    x: ((id as i64 % 80) - 40) as i32,
                    y: ((id as i64 % 37) - 18) as i32,
                    seq,
                };
                seq += 1;
                let payload = msg.encode_to_vec();
                rep.bytes_out +=
                    wire_in_bytes(gsb_game::op::MOVE_TO, payload.len());
                if send_wire(&mut wire, gsb_game::op::MOVE_TO, payload)
                    .await
                    .is_err()
                {
                    break; // peer/session gone early
                }
                rep.moves += 1;
            }
            let timeout = next_move
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(250));
            match recv_wire(&mut wire, timeout).await {
                Got::Frame(_, payload) => {
                    rep.bytes_in += (2 + payload.len()) as u64;
                    if payload.is_empty() {
                        rep.errors += 1;
                    }
                    rep.snapshots += 1;
                }
                Got::Quiet => {}
                Got::Dead => break,
            }
        }

        // -- THE POINT: drop WITHOUT any LEAVE_ROOM_REQ. The reader pump
        //    sees EOF, the connection actor reports ConnClosed, the
        //    registry routes Detach, and the room PARKS the entity
        //    (inside the grace) — the next cycle's join resumes it. The
        //    FINAL session never drops: it keeps playing until the
        //    deadline (a resumed hero under sustained load).
        if final_session {
            break;
        }
        drop(wire);
        drops += 1;

        // Sleep out the rest of the cycle (the "player is away" window).
        tokio::time::sleep(cycle_end.saturating_duration_since(Instant::now())).await;
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
    /// The demo rooms' disconnect-park grace (`None` = config default).
    disconnect_grace_secs: Option<f64>,
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
    if let Some(s) = o.disconnect_grace_secs {
        cfg.disconnect_grace_secs = s.max(0.0);
    }
}

/// Start the server in-process with a channel metrics sink. The receiver
/// moves into the report-drain task; nothing is shared across tasks
/// beyond that mailbox. `visibility` selects the room strategy (the five:
/// `()` / `Cell` / `Team` / `Sector` / sharded-grid).
#[allow(clippy::too_many_arguments)] // loadgen helper; params are natural
async fn start_inprocess(
    visibility: gsb_server::Visibility,
    shard_count: u32,
    cell_size: f32,
    vision_radius: f32,
    max_snapshot_bytes: usize,
    spawn_half: f32,
    transport: gsb_server::TransportKind,
    overrides: ServerOverrides,
) -> Result<InProcessServer, gsb_server::ServerError> {
    let mut cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        visibility,
        shard_count,
        aoi_cell_size: cell_size,
        team_vision_radius: vision_radius,
        max_snapshot_bytes,
        spawn_half_size: spawn_half,
        transport,
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
            eprintln!(
                "mode: external server at {a} (transport={}; client-side numbers only)",
                args.transport
            );
            (a, false, None, None)
        }
        None => {
            let s = start_inprocess(
                args.visibility,
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
                    disconnect_grace_secs: args.disconnect_grace_secs,
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
        tls: args
            .tls_ca
            .clone()
            .map(|ca_path| TlsOpts {
                ca_path,
                server_name: args.tls_server_name.clone(),
            }),
        room: args.room,
        move_ms: args.move_ms,
        stagger_ms: args.stagger_ms,
        profile: args.profile,
        still_frac: args.still_frac,
        spawn_half: args.spawn_half,
        cell_size: args.cell_size,
        deadline,
        flood: false,
        kind: args.transport,
    };
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

/// Total room membership in a report: the SUM over all rooms. For a
/// single room (every non-sharded strategy) this is that room's member
/// count; for a sharded room it is the room's total population (the
/// shards partition the room's connections).
fn report_members(report: &MetricReport) -> u32 {
    report.rooms.iter().map(|r| r.members).sum()
}

/// The report's cumulative step count as a "latest report" proxy: the MAX
/// over all rooms. For a single room that room's steps; for a sharded room
/// the shards step in lockstep (one global ticker) so any shard's count
/// marks the report's recency.
fn report_steps(report: &MetricReport) -> u64 {
    report.rooms.iter().map(|r| r.steps).max().unwrap_or(0)
}

/// Fold a report's rooms into ONE [`RoomReport`] so the (single-room-shaped)
/// print code works for both: a single room (identity fold) and a sharded
/// room (N shard reports). The fold is exact for the single-room case
/// (max/sum/min of one element = that element):
/// - cumulative counters (steps, dropped, snapshots, records, joins, …): SUM
///   (the room's total);
/// - worst-case gauges (step_max, late_max, snap_bytes_max, max_group): MAX
///   (the bottleneck shard);
/// - rates: MIN hz (the slowest shard is the room's rate — the shards step
///   together, so a lagging shard drags the room);
/// - `step_hist`: element-wise SUM (the union of all shards' step
///   distributions, so over-budget % and percentiles are room-wide);
/// - `step_mean_us`: weighted by steps (exact for one shard).
fn fold_rooms(report: &MetricReport) -> Option<RoomReport> {
    let first = report.rooms.first()?;
    if report.rooms.len() == 1 {
        return Some(*first);
    }
    let mut acc = *first;
    let mut total_steps: u128 = 0;
    let mut weighted_mean: f64 = 0.0;
    for r in &report.rooms {
        let s = r.steps as f64;
        total_steps += r.steps as u128;
        weighted_mean += r.step_mean_us * s;
        acc.steps = acc.steps.max(r.steps);
        acc.hz = acc.hz.min(r.hz);
        acc.step_min_us = acc.step_min_us.min(r.step_min_us);
        acc.step_max_us = acc.step_max_us.max(r.step_max_us);
        for i in 0..HIST_BINS {
            acc.step_hist[i] += r.step_hist[i];
        }
        for i in 0..FINE_HIST_BINS {
            acc.step_fine_hist[i] += r.step_fine_hist[i];
        }
        acc.late_max_us = acc.late_max_us.max(r.late_max_us);
        acc.lagged_events += r.lagged_events;
        acc.lagged_ticks += r.lagged_ticks;
        acc.dropped += r.dropped;
        acc.dropped_actions += r.dropped_actions;
        acc.keepalive_resends += r.keepalive_resends;
        acc.snapshots += r.snapshots;
        acc.snap_bytes_max = acc.snap_bytes_max.max(r.snap_bytes_max);
        acc.snap_overflows += r.snap_overflows;
        acc.snap_records += r.snap_records;
        acc.shipped_bytes += r.shipped_bytes;
        acc.groups += r.groups;
        acc.members += r.members;
        acc.max_group = acc.max_group.max(r.max_group);
        acc.joins += r.joins;
        acc.leaves += r.leaves;
        // Reconnect counters (§10): cumulative like joins/leaves → SUM.
        // `detached` is an instant gauge → MAX (the worst shard's park
        // population), mirroring the other gauges above.
        acc.detached = acc.detached.max(r.detached);
        acc.resumes += r.resumes;
        acc.resume_rejected_stale += r.resume_rejected_stale;
        acc.detach_expired_despawn += r.detach_expired_despawn;
        acc.detach_expired_ai += r.detach_expired_ai;
    }
    acc.step_mean_us = if total_steps > 0 {
        weighted_mean / total_steps as f64
    } else {
        0.0
    };
    Some(acc)
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
    let budget_rejected: u64 = reports.iter().map(|r| r.budget_rejected).sum();
    let retrans_out: u64 = reports.iter().map(|r| r.retrans_out).sum();
    let dup_in: u64 = reports.iter().map(|r| r.dup_in).sum();
    let oob_dropped: u64 = reports.iter().map(|r| r.oob_dropped).sum();
    let gave_up: u64 = reports.iter().map(|r| r.gave_up).sum();
    let acks: u64 = reports.iter().map(|r| r.acks).sum();
    let ack_processed_max: u64 = reports.iter().map(|r| r.ack_processed_max).max().unwrap_or(0);
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

    println!("=== gsb loadgen raw report ===");
    println!(
        "machine: cores={cores} profile={profile} mode={} clients_span={}..{}",
        mode_human,
        args.offset,
        args.offset + args.clients.saturating_sub(1)
    );
    println!(
        "clients: transport={} connected={connected}/{} joined={joined} left={left} errors={errors} join_rejected={join_rejected} cap_rejected={cap_rejected} budget_rejected={budget_rejected}",
        args.transport,
        args.clients
    );
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
        fulls, private_fulls, deltas, gap_drops, view_size_total,
        acks, ack_processed_max, ack_lag_max_ms
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
        let p50_fine =
            fine_hist_percentile_us(&r.step_fine_hist, r.steps, 50).unwrap_or(FINE_HIST_CAP_US);
        let p90_fine =
            fine_hist_percentile_us(&r.step_fine_hist, r.steps, 90).unwrap_or(FINE_HIST_CAP_US);
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
        "RESULT mode={} visibility={} shards={} max_snap_bytes={} clients={} connected={} joined={} left={} snap_total={} \
         snap_per_client_p50={:.1} tick_hz_med={:.2} client_in_bps={} client_out_bps={} \
         out_bps_per_conn={:.0} moves={} errors={} steps={} server_hz={:.2} \
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
             detach_expired_ai={} detach_expired_despawn={}",
        mode,
        args.visibility,
        if args.visibility == gsb_server::Visibility::Sharded {
            args.shard_count
        } else {
            1
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
        room.map(|r| r.steps).unwrap_or(0),
        server_hz,
        room
            .map(|r| hist_percentile(&r.step_hist, r.budget_us, r.step_max_us, 0.50))
            .unwrap_or(0.0),
        room
            .map(|r| {
                fine_hist_percentile_us(&r.step_fine_hist, r.steps, 50)
                    .unwrap_or(FINE_HIST_CAP_US)
            })
            .unwrap_or(FINE_HIST_CAP_US),
        room
            .map(|r| {
                fine_hist_percentile_us(&r.step_fine_hist, r.steps, 90)
                    .unwrap_or(FINE_HIST_CAP_US)
            })
            .unwrap_or(FINE_HIST_CAP_US),
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
            Profile::Still => "still",
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
///     [u32; FINE_HIST_BINS] step_fine_hist
///     u64 late_min_us  f64 late_mean_us  u64 late_max_us
///     u64 lagged_events  u64 lagged_ticks  u64 dropped  f64 dropped_s
///     u64 dropped_actions  u64 keepalive_resends  u64 snapshots
///     f64 snap_bytes_s  u32 snap_bytes_max  u64 snap_overflows
///     u64 snap_records  u64 shipped_bytes  f64 shipped_s
///     u32 groups  u32 members  u32 max_group  u64 joins  u64 leaves
///     u64 req_local  u64 req_ext
///     u64 req_rej_malformed  u64 req_rej_dup  u64 req_rej_no_handler
///     u64 req_rej_logic  u64 req_rej_conn  u64 req_rej_room
///     u64 req_to  u64 req_late
///     u32 req_pending
///     u64 metrics_dropped
///   u8 registry_present
///   [if present] u32 rooms  u32 conns  u64 rooms_created
///                u64 rooms_destroyed  u64 rooms_died
///                u64 joins  u64 leaves
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
/// GSM3 = the GSM2 layout plus each room's fine step-duration histogram
/// (`[u32; FINE_HIST_BINS]`, fixed 8 µs bins — sub-budget resolution
/// alongside the budget-relative log2 histogram, whose overflow
/// semantics are untouched).
/// GSM4 = the GSM3 layout plus each room's RPC counters
/// (`req_local / req_ext / req_rej / req_to / req_late` cumulative +
/// `req_pending` gauge, see `gsb_core::rpc` and `MetricReport::rooms`).
/// GSM5 = the GSM4 layout with the single `req_rej` counter replaced by
/// the six per-cause reject buckets (`req_rej_malformed / _dup /
/// _no_handler / _logic / _conn / _room` — one per terminal reject
/// decision in the room's tick body; the buckets answer distinct
/// operational questions, which the cap-sizing measurement needs).
const METRICS_MAGIC: u32 = 0x4753_4D35;

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
        for bin in &room.step_fine_hist {
            w.u32(*bin as u32);
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
        w.u32(room.detached);
        w.u64(room.resumes);
        w.u64(room.resume_rejected_stale);
        w.u64(room.detach_expired_despawn);
        w.u64(room.detach_expired_ai);
        w.u64(room.requests_local);
        w.u64(room.requests_external);
        w.u64(room.requests_rejected_malformed);
        w.u64(room.requests_rejected_dup);
        w.u64(room.requests_rejected_no_handler);
        w.u64(room.requests_rejected_logic);
        w.u64(room.requests_rejected_conn_cap);
        w.u64(room.requests_rejected_room_cap);
        w.u64(room.requests_timed_out);
        w.u64(room.requests_late);
        w.u32(room.pending_requests);
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
        w.u64(g.rooms_died);
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
    w.u64(r.net.violations);
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
        let step_fine_hist = {
            let mut h = [0u64; FINE_HIST_BINS];
            for bin in &mut h {
                *bin = u64::from(r.u32()?);
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
            step_fine_hist,
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
            detached: r.u32()?,
            resumes: r.u64()?,
            resume_rejected_stale: r.u64()?,
            detach_expired_despawn: r.u64()?,
            detach_expired_ai: r.u64()?,
            requests_local: r.u64()?,
            requests_external: r.u64()?,
            requests_rejected_malformed: r.u64()?,
            requests_rejected_dup: r.u64()?,
            requests_rejected_no_handler: r.u64()?,
            requests_rejected_logic: r.u64()?,
            requests_rejected_conn_cap: r.u64()?,
            requests_rejected_room_cap: r.u64()?,
            requests_timed_out: r.u64()?,
            requests_late: r.u64()?,
            pending_requests: r.u32()?,
            metrics_dropped: r.u64()?,
        });
    }
    let registry = match r.u8()? {
        1 => Some(RegistryReport {
            rooms: r.u32()?,
            conns: r.u32()?,
            rooms_created: r.u64()?,
            rooms_destroyed: r.u64()?,
            rooms_died: r.u64()?,
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
        violations: r.u64()?,
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
        // The emission stamp (`emitted_at`) is a monotonic-clock `Instant`
        // from the SERVER process — meaningless across a process boundary
        // (the orchestrator's timeline differs), so it does not ride the
        // wire format (unchanged "GSM1"). The decoder stamps ARRIVAL time:
        // good enough for the orchestrator's age-style uses and honest
        // about when this side first held the value.
        emitted_at: Instant::now(),
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
        shard_count: args.shard_count,
        aoi_cell_size: args.cell_size,
        team_vision_radius: args.vision_radius,
        max_snapshot_bytes: args.max_snapshot_bytes,
        spawn_half_size: args.server_spawn_half,
        transport: args.transport,
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
            disconnect_grace_secs: args.disconnect_grace_secs,
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
    budget_rejected: u64,
    retrans_out: u64,
    dup_in: u64,
    oob_dropped: u64,
    gave_up: u64,
    acks: u64,
    ack_processed_max: u64,
    ack_lag_max_ms: u128,
    fulls: u64,
    private_fulls: u64,
    deltas: u64,
    gap_drops: u64,
    view_size: u64,
    churn_cycles: u64,
    resumed: u64,
    fresh_joins: u64,
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
        budget_rejected: get("budget_rejected")?.parse().ok()?,
        retrans_out: get("retrans_out")?.parse().ok()?,
        dup_in: get("dup_in")?.parse().ok()?,
        oob_dropped: get("oob_dropped")?.parse().ok()?,
        gave_up: get("gave_up")?.parse().ok()?,
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
        "--shard-count".into(),
        args.shard_count.to_string(),
        "--cell-size".into(),
        args.cell_size.to_string(),
        "--vision-radius".into(),
        args.vision_radius.to_string(),
        "--max-snapshot-bytes".into(),
        args.max_snapshot_bytes.to_string(),
        "--spawn-half-size".into(),
        args.server_spawn_half.to_string(),
        "--transport".into(),
        args.transport.to_string(),
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
    if let Some(f) = args.disconnect_grace_secs {
        sargs.push("--disconnect-grace-secs".into());
        sargs.push(f.to_string());
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
    // Readiness: a TCP connect probe on tcp; on udp there is no SYN to
    // probe with — one cookie-handshake CHALLENGE (a bare challenge
    // request establishes nothing on the server).
    let probe_sock = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("probe socket binds");
    loop {
        let ready = if args.transport == gsb_server::TransportKind::Udp {
            UdpClient::challenge_probe(&probe_sock, server_addr, Duration::from_millis(200))
                .await
        } else {
            match TcpStream::connect(server_addr).await {
                Ok(mut s) => {
                    let _ = s.shutdown().await;
                    true
                }
                Err(_) => false,
            }
        };
        if ready {
            break;
        }
        if Instant::now() >= probe_deadline {
            eprintln!("orchestrate: server socket not ready after 10 s; clients will report their own connect failures");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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
                Profile::Still => "still".into(),
            },
            "--still-frac".into(),
            args.still_frac.to_string(),
            "--spawn-half-size".into(),
            args.spawn_half.to_string(),
            "--transport".into(),
            args.transport.to_string(),
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
        if let Some(c) = args.churn_secs {
            cargs.push("--churn-secs".into());
            cargs.push(c.to_string());
        }
        if args.churn_cycles != 0 {
            cargs.push("--churn-cycles".into());
            cargs.push(args.churn_cycles.to_string());
        }
        if let Some(f) = args.disconnect_grace_secs {
            cargs.push("--disconnect-grace-secs".into());
            cargs.push(f.to_string());
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
            budget_rejected: c.budget_rejected,
            retrans_out: c.retrans_out,
            dup_in: c.dup_in,
            oob_dropped: c.oob_dropped,
            gave_up: c.gave_up,
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
