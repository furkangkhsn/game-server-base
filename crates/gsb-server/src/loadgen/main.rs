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
//! gsb-loadgen [N] [--game demo|arena|mmo|war] [--duration SECS] [--move-ms MS] [--room ID]
//!              [--stagger-ms MS] [--visibility all|spatial|team|pvs] [--cell-size N]
//!              [--vision-radius N]
//!              [--max-snapshot-bytes N] [--addr HOST:PORT]
//! ```
//! Defaults: N=100, duration=10 s, move interval 150 ms, room 1,
//! stagger 0 (all clients connect at once — the worst case for the accept
//! path over loopback; see `Args::stagger_ms`), visibility `all`
//! (same default as the server config; see `gsb_server::Visibility`).
//!
//! Every client: connects, authenticates, joins the room, then until the
//! deadline sends one numbered input per `--move-ms` — what the input is
//! is the game's bot (`bot/`, `--game`; the demo's: a `MOVE_TO` around a
//! circle, phase-shifted by client id so the entities do not move in
//! lockstep) — and applies what it receives into its view.
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

mod bot;
mod capture;
mod churn;
mod client;
mod codec;
mod orchestrate;
mod report;
mod run;
mod serve;
mod server;
mod stats;
mod transport;

use orchestrate::*;
use run::*;
use serve::*;
use transport::Transport;

use std::time::Duration;

mod args;
use args::*;

/// The 2D demo's client movement profile (the load's *shape*, not the
/// server's visibility strategy; the other games' bots move their own
/// way — `bot/`):
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
    /// The game to drive (`--game`, default `demo`; a catalog name — see
    /// `bot::games`): picks the clients' bot, is the in-process / served
    /// server's `game` key, and closes the RESULT line (`game=<name>`).
    game: &'static str,
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
    /// (10k clients × 0.5 ms = 5 s of spread). The other cure for a
    /// storm that overflows the server's accept queue (B84) is a larger
    /// `--listen-backlog`: it keeps the storm (the admission path's worst
    /// case) and only stops the kernel from dropping its SYNs; a stagger
    /// removes the storm — use it when the run should model joins spread
    /// over time, and mind that the spread comes out of `--duration`.
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
    /// The EXPLICIT topology axis of the served / in-process server
    /// (`--topology single|sharded`; the Faz A explicit-key surface).
    /// `None` (default) = derive from the legacy visibility spelling, so
    /// every pre-flag invocation behaves identically. The composite
    /// selection is expressed as `--visibility spatial --topology
    /// sharded` — per-shard cell-grouped AOI broadcast (ROADMAP Faz B) —
    /// which the legacy spelling alone cannot name.
    topology: Option<gsb_server::Topology>,
    /// Shards per room (`--shard-count N`, default 4; used only for
    /// `sharded`). The map is a near-square grid of N shards; 1..=256.
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
    /// orchestrator — no report is parsed from stdout. Stdout carries one
    /// `SERVING` line once every door is bound, naming the bound
    /// addresses (port 0 included; BACKLOG F31, `serve::announce`).
    serve: bool,
    /// Bind address for `--serve` (default 127.0.0.1:7777; the
    /// orchestrator's server child gets port 0).
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
    /// Session-lifecycle write-stall window of the in-process / served
    /// server, in seconds (`--write-stall-secs F`; 0 = disabled;
    /// unspecified = the server config default, 10 s). The other
    /// direction's twin of `idle_timeout_secs`: how long the server's
    /// writer may go without writing to a socket before it ends that
    /// session.
    write_stall_secs: Option<f64>,
    /// Capacity of each connection's outbound batch channel on the
    /// in-process / served server (`--conn-out N`; unspecified = the
    /// server config default, 256). The fan-out drops a connection's
    /// batch when its channel is full: a small value makes a reader that
    /// falls a few ticks behind lose batches (the F11 measurement).
    conn_out: Option<usize>,
    /// The accept backlog of the in-process / served server's TCP-based
    /// door (`--listen-backlog N`, BACKLOG B84; unspecified = the server
    /// config default, 128 — tokio's own). A join storm of N clients
    /// with default workers overflows a 128 queue (Linux counts it as
    /// `TcpExtListenOverflows`); the overflowed clients retry after a
    /// second and a short run can end before they join. Give it at least
    /// the client count (the kernel caps it at `somaxconn`), or spread
    /// the connects with `--stagger-ms`.
    listen_backlog: Option<u32>,
    /// The receive buffer of the in-process / served server's UDP-based
    /// door (`--udp-recv-buffer N`, BACKLOG B4; unspecified = the server
    /// config default: untouched, the system's `rmem_default`). A rUDP
    /// join storm overflows the one socket's receive queue (Linux counts
    /// it as `Udp: RcvbufErrors`); the lost handshake datagrams are
    /// re-sent (`hs_retries`) and connect p99 grows. Linux caps it at
    /// `net.core.rmem_max`.
    udp_recv_buffer: Option<u32>,
    /// The in-process / served server's rUDP congestion response
    /// (`--udp-congestion off|pace`, BACKLOG B104; unspecified = the
    /// server config default, `off`): `pace` paces a reporting session's
    /// game band to its path's estimated rate. The jitter measurement
    /// (`scripts/rudp-jitter.sh`) runs the same load under both.
    udp_congestion: Option<gsb_server::UdpCongestionKind>,
    /// The rUDP record layer (`--udp-security sealed|plaintext`, default
    /// sealed — the server's own default, BACKLOG B5a): the in-process /
    /// served server's `udp_security`, with an ephemeral static key it
    /// draws at start when sealed, and what the clients speak. Its clients
    /// pin the public half (in process: the server handle's; served: the
    /// `SERVING` line's `udp_key=`; external: `--udp-server-key`).
    udp_security: gsb_server::UdpSecurityKind,
    /// The external sealed server's public key the rUDP clients pin
    /// (`--udp-server-key HEX`, 64 hex characters; `--addr` runs only —
    /// the orchestrator passes its served child's).
    udp_server_key: Option<[u8; 32]>,
    /// The in-process / served sealed rUDP door's handshake budget
    /// (`--udp-handshakes-per-sec N`, BACKLOG B119; unspecified = the
    /// server config default, 1000; `0` = none).
    udp_handshakes_per_sec: Option<u32>,
    /// The GLOBAL id of the client that floods (`--flood-id K`): after
    /// joining it writes MOVE_TO frames in a tight loop (as fast as the
    /// socket accepts) until the deadline — the input-flood behaviour
    /// probe for the fairness / drop-attribution guards.
    flood_id: Option<u64>,
    /// The transport the clients speak (and the in-process / served
    /// server listens on): `--transport tcp|udp|ws` (default `tcp`). On
    /// `udp` the client's `connect_ms` is the rUDP cookie-HANDSHAKE
    /// latency (challenge + proof), not a TCP handshake; on `ws` it is
    /// the TCP connect plus the HTTP upgrade's round trip.
    transport: Transport,
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
    /// The fraction of MMO bots that duel across a shard seam
    /// (`--mmo-duel-frac F`, default 0: the default bot only — see
    /// `bot/mmo/duel.rs`).
    mmo_duel_frac: f64,
    /// The in-process / served MMO's `[mmo] crystallize`
    /// (`--mmo-crystallize on|off`; unspecified = the MMO's default, on).
    mmo_crystallize: Option<bool>,
    /// Record the raw game-band frames of a sample of clients into this
    /// directory, one file per client (`--capture DIR`; `capture.rs` —
    /// an offline wire study's input). Plain in-process or `--addr` runs
    /// only.
    capture: Option<String>,
    /// How many clients `--capture` records (`--capture-clients K`,
    /// default 8), spread evenly over the run's ids.
    capture_clients: u64,
    /// The slow reader (`--stall-ms MS`): every client stops reading for
    /// MS once per `--stall-every-ms` period (phase-staggered by id), on
    /// a small socket receive buffer — the server's writer then blocks
    /// and the fan-out drops that client's batches (the F11 measurement).
    stall_ms: Option<u64>,
    /// The slow reader's period (`--stall-every-ms MS`, default 5000).
    stall_every_ms: u64,
    /// The RPC traffic mode (`--rpc-rate R`, BACKLOG B23): every client
    /// sends R correlated requests per second (the demo's `ECONOMY`)
    /// beside its inputs and matches the answers (`client/rpc.rs`).
    rpc_rate: Option<f64>,
    /// Requests per burst of the RPC mode (`--rpc-burst B`, default 1):
    /// B back to back every B / R seconds — above the per-connection
    /// pending cap (4) a burst meets the cap's rejections.
    rpc_burst: Option<u32>,
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

Game:
  --game demo|arena|mmo|war the game to drive (default demo): the clients'
                            bot and the in-process/served server's `game`
                            key. demo = the 2D demo (profiles below);
                            arena = the 3D team arena (units run base →
                            centre → base, climbing); mmo = the 3D sharded
                            MMO (roam a waystone, Travel, Attack); war =
                            Cephe, three factions over a sharded map with
                            team fog (hold a tower or capture point, move
                            on, Attack). The flags marked [demo] refuse the
                            other games

Client options:
  --addr HOST:PORT          connect to an external server (default: start
                            an in-process server)
  --duration SECS           run duration (default 10)
  --move-ms MS              MOVE_TO interval (default 150)
  --room ID                 room id to join (default 1)
  --stagger-ms MS           client i connects i×ms later (default 0)
  --offset K                first client id (default 0; client i = K+i)
  --transport tcp|udp|ws    client transport (default tcp). udp = rUDP:
                            stateless cookie handshake, reliable control
                            band, loss-tolerant snapshot band; connect_ms
                            then measures the handshake. ws = WebSocket
                            (one frame per binary message; the server
                            gets one ws listener); connect_ms includes
                            the HTTP upgrade, client bytes the WS framing
  --profile ring|spread|still [demo] movement profile (default ring — the historical
                            clustered layout; spread = uniform over the
                            ±spawn-half map, the sparse MOBA-like layout;
                            still = a configurable stillness ratio: the still
                            clients settle once, the minority moves)
  --still-frac F            [demo] fraction of still clients for the still profile
                            (default 0.9; deterministic per-id split)
  --stall-ms MS             slow readers: every client stops reading for MS
                            once per --stall-every-ms (default 5000),
                            staggered by id, on a 16 KiB TCP receive buffer
  --rpc-rate R              [demo] RPC traffic: every client sends R
                            requests/s (the demo's ECONOMY purchase) beside
                            its inputs and matches the answers (RESULT's
                            rpc_* keys); plain client runs only
  --rpc-burst B             [demo] requests per burst (default 1): B back to
                            back every B/R s (above 4 — the per-connection
                            pending cap — a burst meets the cap)
  --spawn-half-size F       [demo] map half-size for the spread profile's homes
                            and the (in-process/served) server's spawn
                            points (default: 50 for ring, 1000 for spread)
  --workers N               tokio worker threads for this process (default:
                            available parallelism); the orchestrator
                            forwards an explicit N to unpinned children

Server options (in-process server, --serve, or the orchestrator's server):
  --visibility all|spatial|team|pvs|sharded   [demo] (default all)
  --topology single|sharded           [demo] explicit topology axis; with
                                       --visibility spatial selects the
                                       sharded × spatial composite
  --shard-count N                     [demo] (sharded; near-square grid of N
                                       shards, 1..=256; default 4 = 2×2)
  --cell-size F                       [demo] (spatial; default 20)
  --vision-radius F                   [demo] (team; default 25)
  --max-snapshot-bytes N              (default 1400)
  --max-players N                     per-room membership cap (0 = unlimited;
                                       default: the server config default,
                                       10 000 — the measured single-room wall)
  --max-connections N                 server-wide connection cap (0 =
                                       unlimited; default: the server config
                                       default, 100 000)
  --idle-timeout-secs F               idle session window (0 = disabled;
                                       default: the server config default, 30)
  --write-stall-secs F                write-stall window: a session the
                                       server cannot write to for F seconds
                                       is ended (0 = disabled; default: the
                                       server config default, 10)
  --conn-out N                        per-connection outbound batch
                                       capacity (default: the server config
                                       default, 256); small = fan-out drops
  --listen-backlog N                  accept backlog of the server's door
                                       (default: the server config default,
                                       128; the kernel caps it at
                                       somaxconn). A join storm of more
                                       clients than this overflows it and
                                       the overflowed ones retry after 1 s:
                                       give it >= N, or use --stagger-ms
  --udp-security sealed|plaintext     the rUDP record layer (default sealed,
                                       the server's default: the in-process
                                       or served server draws an ephemeral
                                       static key and its clients pin it;
                                       plaintext = the dev/LAN door)
  --udp-handshakes-per-sec N          the server's sealed rUDP handshake
                                       budget (default: the config's, 1000;
                                       0 = none)
  --udp-server-key HEX                the external sealed server's public
                                       key (64 hex; with --addr and
                                       --transport udp)
  --udp-congestion off|pace           the server's rUDP congestion response
                                       (default: the config's, off; pace =
                                       pace a reporting session's game band
                                       to its path's estimated rate)
  --udp-recv-buffer N                 receive buffer (SO_RCVBUF, bytes) of
                                       the server's UDP door (default: the
                                       server config default, untouched;
                                       Linux caps it at net.core.rmem_max).
                                       A rUDP join storm past it loses
                                       handshake datagrams (RcvbufErrors)

  --disconnect-grace-secs F           [demo] disconnect-park grace (default:
                                       the server config default, 30)
  --mmo-crystallize on|off            [mmo] crystallize cross-seam fights
                                       (default: the MMO's, on)

Client behaviour:
  --flood-id K                        client K floods its game's move input in
                                      a tight loop after joining (the
                                      input-flood probe)
  --churn-secs S                      every client drops (no leave) and
                                      resumes every S seconds
  --churn-cycles K                    drop→resume transitions per client
                                      (0 = until the deadline)
  --mmo-duel-frac F                   [mmo] fraction of the bots (in id
                                      pairs) that duel across the x = 0
                                      shard seam instead of roaming
                                      (default 0)

Measurement:
  --capture DIR                       record the game-band frames (group
                                      snapshots, private frames) and the
                                      join result a sample of clients
                                      received, one
                                      `client-<id>.gsbcap` file each
                                      (format: src/loadgen/capture.rs);
                                      plain client runs only
  --capture-clients K                 how many clients --capture records,
                                      spread evenly over the ids (default 8)

Orchestrator options (--orchestrate):
  --procs P             client process count (default 1)
  --pin                 pin children to disjoint cores via taskset
  --pin-server-cores C  physical cores for the server under --pin (default 8)

Server-only options (--serve):
  --bind HOST:PORT          (default 127.0.0.1:7777)
  --metrics-listen HOST:PORT  stream metric reports (binary, channel data)
                              to one connecting orchestrator; without it,
                              reports go to the gsb-metric log
  Once bound, prints `SERVING addr=HOST:PORT metrics=HOST:PORT|-` on
  stdout (port 0 binds a free port; the line names it)

Misc:
  -h, --help                this text";

fn main() {
    // A refused command line is a message and exit status 2 (a usage
    // error), never a panic: the reason is the whole diagnosis.
    let args = match parse_args() {
        Ok(Cli::Run(args)) => *args,
        Ok(Cli::Help) => {
            println!("{USAGE}");
            return;
        }
        Err(e) => {
            eprintln!("gsb-loadgen: {e}");
            std::process::exit(2);
        }
    };
    // Default worker count = available_parallelism: `workers = 0` used to
    // mean a SINGLE worker, which at ≥1000 in-process clients starved the
    // room actor between ticks (measured: 19-22 Hz with sub-ms steps and
    // ~200 ms late_max; 8 workers restore 30.00 Hz / drop 0 / p50 782 µs —
    // see ROADMAP "regresyon ölçüm turu"). An explicit --workers still wins.
    let workers = if args.workers > 0 {
        args.workers
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(workers)
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
