//! gsb composition root.
//!
//! [`start_server`] wires the whole stack together: the transport (default
//! TCP, pluggable), the registry actor (control plane), the pre-created
//! rooms (via the game crate's [`gsb_game::room::DemoRoom`]), and the accept
//! loop. It must be called from inside a tokio runtime.
//!
//! ```text
//! global ticker (broadcast) ──TickInfo──▶ room actors (5-phase tick)
//!                                            │ per-conn action channels (in)
//! accept loop ──ConnOpened──▶ registry actor ◀──RegistryMsg── connection actors
//!     │                           │CreateRoom/SpawnPlayer/…      │ try_send
//!     ▼                           ▼                               ▼
//! endpoint pumps             (control plane)              room actor (pulls)
//! (reader/writer per conn)                               │
//!                                                        ▼
//!                                         World (bevy_ecs) + RoomLogic (gsb-game)
//! ```

use std::net::SocketAddr;
use std::sync::Arc;

use bevy_ecs::world::World;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use gsb_core::channel::channel;
use gsb_core::conn::ConnectionActor;
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::{MetricReport, MetricSink, MetricsCollector, MetricsEvent};
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{RoomConfig, RoomLogic};

/// The visibility strategy of the demo rooms (config-selectable; all run
/// the SAME game — same components, movement, wire format — and differ
/// only in how the world is partitioned into snapshot groups, see
/// `docs/DESIGN.md` §8).
///
/// Each variant is a different `RoomLogic` group key, so each needs its
/// own registry instantiation (a `RoomFactory` is generic over the group
/// key — the pick happens at this config boundary, entirely on the
/// server side; `gsb-core` stays generic and untouched).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// `GroupKey = ()`: everyone sees the whole world. The baseline
    /// (per-connection bandwidth O(entities)); the comparison point all
    /// other strategies are measured against.
    All,
    /// `GroupKey = Cell`: spatial AOI (3×3 cell block, see
    /// [`gsb_game::aoi`]).
    Spatial,
    /// `GroupKey = Team`: team fog of war (2 groups, team vision; see
    /// [`gsb_game::team`]).
    Team,
    /// `GroupKey = Sector`: per-map-segment PVS (static visibility table
    /// over hand-authored convex sectors; see [`gsb_game::pvs`]).
    Pvs,
    /// Grid of shards (see [`gsb_game::sharded`]): the room is `shard_count`
    /// actors, each owning a rectangular region of the map, with entity
    /// migration across region boundaries and boundary visibility. This is
    /// a different *topology* (N actors + N worlds), not just a group key,
    /// so it plugs in via `ShardLogic`/`BuiltRoom::Sharded` rather than
    /// `RoomLogic`. Selecting it uses [`Config::shard_count`] (1..=16).
    Sharded,
}

impl Default for Visibility {
    /// Default: `All` — the whole-world baseline. Rationale (see
    /// `docs/ROADMAP.md`, visibility turn): (1) it is the backward-
    /// compatible behavior of every existing config (previously
    /// `aoi = false`); (2) it is the measurement baseline all restricted
    /// strategies are compared against — a restricted default would make
    /// the baseline opt-in; (3) for a new server author, "everyone sees
    /// everything" is the least-surprising starting point: restricting
    /// what clients can see is a *gameplay* decision and should be an
    /// explicit choice, not a default.
    fn default() -> Self {
        Self::All
    }
}

impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::All => "all",
            Self::Spatial => "spatial",
            Self::Team => "team",
            Self::Pvs => "pvs",
            Self::Sharded => "sharded",
        };
        f.write_str(s)
    }
}
use gsb_net::tcp::TcpTransport;
use gsb_net::transport::Transport;
use gsb_net::udp::{UdpTransport, UdpTransportConfig};
use gsb_protocol::MessageTable;

/// The wire transport (see `config.example.toml` and `docs/DESIGN.md` §6).
///
/// Both transports sit behind the same `gsb_net::transport` traits, so
/// the actor layer is identical for either; they differ in the wire
/// protocol and the session lifecycle:
///
/// - `Tcp`: one socket per connection, length-prefixed frames, stream
///   semantics (the reader pump's idle timeout is the teardown guard).
/// - `Udp`: rUDP — one socket for every session, a stateless cookie
///   handshake (anti-amplification), a reliable control band over a
///   loss-tolerant snapshot band, a datagram budget, and idle teardown
///   via the demux's deadline heap (no FIN in UDP). See `gsb_net::udp`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    /// Length-prefixed TCP (the default).
    #[default]
    Tcp,
    /// rUDP (one shared socket, one shared demux).
    Udp,
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        };
        f.write_str(s)
    }
}

/// Server configuration (see `config.example.toml`).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Socket address to bind (e.g. `"0.0.0.0:7777"`, `"127.0.0.1:0"`).
    pub bind: String,
    /// Global tick rate in ticks per second. Every room must run at a rate
    /// that divides this one (a room at `global / k` steps every k-th tick).
    pub tick_hz: f64,
    /// Number of rooms to pre-create at startup (ids `1..=room_count`).
    pub room_count: u64,
    /// Maximum frame body size in bytes (transport-level guard).
    pub max_frame_bytes: usize,
    /// Capacity of each room's control channel (join/leave/shutdown).
    pub room_control: usize,
    /// Capacity of each connection's action channel (inputs buffered until
    /// the room's next tick).
    pub conn_action: usize,
    /// Capacity of each connection's inbound (frames in) mailbox.
    pub conn_inbox: usize,
    /// Capacity of each connection's outbound (batches out) channel.
    pub conn_out: usize,
    /// Session-lifecycle idle window, in seconds: a connection that sends
    /// *no* inbound frame (heartbeat, input, anything) for this long is
    /// closed by the server on its own initiative (a gentle `ERROR` frame,
    /// code 9, then EOF). This is the half-open-TCP guardrail — a client
    /// whose cable was pulled or power lost sends no FIN/RST, and its
    /// 3 tasks + 2 channels + registry entry would otherwise sit until
    /// process death. `0` disables the check.
    ///
    /// Default 30 s: comfortably above the ~10 s heartbeat cadence a
    /// well-behaved client should keep (any inbound frame resets the
    /// window, so a live client can never trip it), and short enough that
    /// a dead-but-open connection is detected within a minute. Clients
    /// that never send *anything* (not even heartbeats) must stay below
    /// this with whatever traffic they do send.
    pub idle_timeout_secs: f64,
    /// Per-room membership cap (see `RoomConfig::max_players`); a join
    /// into a full room is rejected with `ERROR` code 8 (the connection
    /// stays alive). `None` = unlimited.
    pub max_players: Option<u32>,
    /// Server-wide connection cap, enforced at connection birth by the
    /// registry (the count lives in its table, so the guardrail does —
    /// the accept loop cannot see disconnects without a second awaited
    /// source). A rejected connection gets `ERROR` code 9 + EOF and no
    /// registry entry. `None` = unlimited.
    ///
    /// Default 100_000: the design goal itself (DESIGN §1, "100k+ concurrent
    /// connections") as a hard guardrail — beyond the goal is an unmeasured
    /// region, and the cap keeps the server's behavior there defined (gentle
    /// rejection) instead of unbounded resource growth.
    pub max_connections: Option<u64>,
    /// Warn when a room group's snapshot payload exceeds this many bytes
    /// (rUDP MTU readiness; default = `max_frame_bytes`).
    pub max_snapshot_bytes: usize,
    /// Keep-alive rate for unchanged snapshot groups, in Hz (a client that
    /// lost its last snapshot must not stay stale forever). `<= 0` disables.
    pub keepalive_hz: f64,
    /// The visibility strategy of the demo rooms (see [`Visibility`]).
    pub visibility: Visibility,
    /// Number of shards per room (used only when
    /// [`Self::visibility`] = [`Visibility::Sharded`]). The map is divided
    /// into a near-square grid of `rows × cols` shards (`rows * cols =
    /// shard_count`, see [`gsb_game::sharded::grid_shape`]). Must be
    /// 1..=16 (the grid topology); validated at startup. Default 4 (2×2).
    pub shard_count: u32,
    /// World units per AOI cell edge (used only when
    /// [`Self::visibility`] = `Spatial`). See `gsb_game::aoi` for the
    /// `max_snapshot_bytes` / density relation and the measured break-even.
    /// The transport (see [`TransportKind`]).
    pub transport: TransportKind,
    /// The rUDP datagram budget in bytes (transport-level guard; default
    /// 1472 = MTU 1500 − IP 20 − UDP 8). Used only when
    /// [`Self::transport`] = `Udp`. See `gsb_net::udp` (feature 3:
    /// oversized frames are dropped and counted, never fragmented).
    pub udp_max_datagram_bytes: usize,
    /// The rUDP cookie key as 32 hex characters (16 bytes). Used only
    /// when [`Self::transport`] = `Udp`. `None` (the default) = draw the
    /// key from the OS entropy source at bind time; if that draw fails
    /// the server refuses to start — a predictable key would invert the
    /// handshake's anti-amplification property (see `gsb_net::udp`).
    pub udp_cookie_key: Option<String>,
    /// World units per AOI cell edge (used only when
    /// [`Self::visibility`] = `Spatial`). See `gsb_game::aoi` for the
    /// `max_snapshot_bytes` / density relation and the measured break-even.
    pub aoi_cell_size: f32,
    /// World units an enemy must be within to be visible to a team (used
    /// only when [`Self::visibility`] = `Team`). See `gsb_game::team` for
    /// the vision source model.
    pub team_vision_radius: f32,
    /// Half-size of the square map entities spawn on (all four demo rooms;
    /// default 50 = the historical 100×100 arena, bit-identical). A load
    /// profile that places entities on a "wide map" (the generator's
    /// `spread` profile) pairs a large value here with the same value in
    /// the clients' `--spawn-half-size`, so spawn points and targets live
    /// on the same map and the run is statistically steady from tick 1.
    /// The PVS strategy's *visibility map* stays its hand-authored 100×100
    /// sectors regardless (see `gsb_game::pvs::SectorRoom::spawn_half`).
    pub spawn_half_size: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7777".into(),
            tick_hz: 30.0,
            room_count: 1,
            max_frame_bytes: gsb_net::tcp::DEFAULT_MAX_FRAME_BYTES,
            room_control: 128,
            conn_action: 256,
            conn_inbox: 1024,
            conn_out: 256,
            idle_timeout_secs: 30.0,
            max_players: Some(10_000),
            max_connections: Some(100_000),
            max_snapshot_bytes: gsb_net::tcp::DEFAULT_MAX_FRAME_BYTES,
            keepalive_hz: 1.0,
            visibility: Visibility::default(),
            shard_count: 4,
            transport: TransportKind::default(),
            udp_max_datagram_bytes: gsb_net::udp::DEFAULT_MAX_DATAGRAM_BYTES,
            udp_cookie_key: None,
            aoi_cell_size: 20.0,
            team_vision_radius: gsb_game::team::DEFAULT_VISION_RADIUS,
            spawn_half_size: gsb_game::room::DEFAULT_SPAWN_HALF,
        }
    }
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let mut cfg: Self = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.display().to_string(),
            source: e,
        })?;
        // Config-file convenience: an explicit 0 means "unlimited" for the
        // caps (a cap of 0 would be a room/server nobody can enter). This
        // mirrors the loadgen CLI semantics (`--max-players 0` etc.).
        // Omitting the key keeps the built-in default (see `Default`);
        // `idle_timeout_secs = 0` is already handled at use time.
        if cfg.max_players == Some(0) {
            cfg.max_players = None;
        }
        if cfg.max_connections == Some(0) {
            cfg.max_connections = None;
        }
        Ok(cfg)
    }
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot parse config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
}

/// Errors produced while starting the server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("invalid bind address `{0}`: {1}")]
    BadBind(String, String),

    #[error("transport bind failed: {0}")]
    Bind(#[from] std::io::Error),

    #[error("invalid `udp_cookie_key` in config: {0}")]
    BadCookieKey(String),

    #[error("invalid `shard_count` {0}: must be 1..=16 (grid topology)")]
    BadShardCount(u32),
}

/// Parse the config's 32-hex-char cookie key into 16 bytes (the rUDP
/// cookie key is an operator-supplied alternative to the OS-entropy
/// draw — see `gsb_net::udp::CookieKey`).
fn parse_cookie_key(s: &str) -> Result<[u8; 16], String> {
    if s.len() != 32 {
        return Err(format!("expected 32 hex characters, got {}", s.len()));
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| {
            format!(
                "invalid hex pair `{}:{}` at position {}",
                &s[i * 2..i * 2 + 1],
                &s[i * 2 + 1..i * 2 + 2],
                i * 2
            )
        })?;
    }
    Ok(out)
}

/// Handle to a running server.
pub struct ServerHandle {
    registry: gsb_core::channel::Mailbox<RegistryMsg>,
    accept: JoinHandle<()>,
    ticker: JoinHandle<()>,
    /// The metrics collector (emits one final report when the ticker's
    /// broadcast closes).
    metrics: JoinHandle<()>,
    /// The bound listener. `stop` closes it *before* aborting the accept
    /// loop: for the rUDP transport this is what stops the shared demux
    /// task (a plain drop would not reach it — see `Listener::close`).
    listener: Arc<dyn gsb_net::transport::Listener>,
    /// The actual bound address (useful when binding port 0 in tests).
    pub addr: SocketAddr,
}

impl ServerHandle {
    /// Shut the server down: the registry tears down connections and rooms
    /// (rooms get a control `Shutdown`, processed on their next tick); the
    /// ticker is aborted, which closes the broadcast and stops any room that
    /// missed its window; the listener is closed (stopping any transport
    /// shared state, e.g. the rUDP demux); the accept loop is hard-aborted
    /// (documented v1 limitation). The metrics collector is awaited last:
    /// it emits one final report when the broadcast closes.
    pub async fn stop(self) {
        let _ = self.registry.send(RegistryMsg::Shutdown).await;
        self.ticker.abort();
        self.listener.close();
        self.accept.abort();
        let _ = self.metrics.await;
    }
}

/// The demo composition: base protocol + demo game messages.
pub fn build_table() -> Arc<MessageTable> {
    let mut table = gsb_protocol::base_table();
    gsb_game::register(&mut table);
    Arc::new(table)
}

/// The room factory for the demo game: an empty bevy `World` + a
/// [`gsb_game::room::DemoRoom`] over a spawn map of half-size
/// `spawn_half`. Group key is `()` (one group per room) — the AOI-**off**
/// baseline: every connection receives the whole world.
fn demo_room_factory(spawn_half: f32) -> RoomFactory<World, (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(gsb_game::room::DemoRoom::with_spawn_half(spawn_half))
            as Box<dyn RoomLogic<World, GroupKey = ()>>,
    })
}

/// The AOI room factory: an empty bevy `World` + an
/// [`gsb_game::aoi::AoiRoom`] with the given `cell_size` (world units per
/// cell edge). Group key is a spatial [`gsb_game::aoi::Cell`] — the
/// spatial path: one snapshot per cell, shared by reference with the
/// cell's occupants. Note the `RoomFactory`'s group-key associated type
/// differs from `demo_room_factory`'s (`Cell` vs `()`), so the strategies
/// cannot be stored in one value — `start_inner` picks the factory at the
/// config boundary. This is entirely on the game/server side; `gsb-core`
/// stays generic over the group key and is untouched.
fn aoi_room_factory(
    cell_size: f32,
    spawn_half: f32,
) -> RoomFactory<World, gsb_game::aoi::Cell, ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(gsb_game::aoi::AoiRoom::with_spawn_half(cell_size, spawn_half))
            as Box<dyn RoomLogic<World, GroupKey = gsb_game::aoi::Cell>>,
    })
}

/// The team-fog room factory: an empty bevy `World` + a
/// [`gsb_game::team::TeamRoom`] with the given `vision_radius`. Group key
/// is [`gsb_game::team::Team`] (2 groups).
fn team_room_factory(
    vision_radius: f32,
    spawn_half: f32,
) -> RoomFactory<World, gsb_game::team::Team, ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(gsb_game::team::TeamRoom::with_spawn_half(vision_radius, spawn_half))
            as Box<dyn RoomLogic<World, GroupKey = gsb_game::team::Team>>,
    })
}

/// The PVS room factory: an empty bevy `World` + a
/// [`gsb_game::pvs::SectorRoom`] (the demo map is built into the room).
/// Group key is [`gsb_game::pvs::Sector`].
fn pvs_room_factory(spawn_half: f32) -> RoomFactory<World, gsb_game::pvs::Sector, ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(gsb_game::pvs::SectorRoom::with_spawn_half(spawn_half))
            as Box<dyn RoomLogic<World, GroupKey = gsb_game::pvs::Sector>>,
    })
}

/// The sharded room factory: `shard_count` shards, each an empty bevy
/// [`World`] + a [`gsb_game::sharded::ShardedRoom`] over the same map
/// (half-size `spawn_half`). Unlike the other factories (which build one
/// `RoomLogic` room), this returns [`BuiltRoom::Sharded`]: N shard worlds
/// and N `ShardLogic` instances (the grid topology is the room's business,
/// the registry just wires the channels).
///
/// `home_shard` routes a join to the shard owning the joiner's *spawn*
/// position (the deterministic `spawn_pos` → the grid region of that
/// point). The router is pure and synchronous (no await) — the registry
/// never blocks on it.
fn sharded_room_factory(
    spawn_half: f32,
    shard_count: usize,
) -> RoomFactory<World, (), gsb_game::sharded::ShardedRoomState> {
    Arc::new(move |_id, _config| {
        let shards: Vec<gsb_core::registry::Shard<World, (), gsb_game::sharded::ShardedRoomState>> =
            (0..shard_count)
                .map(|i| {
                    (
                        World::new(),
                        Box::new(gsb_game::sharded::ShardedRoom::new(
                            i,
                            shard_count,
                            spawn_half,
                        ))
                            as Box<
                                dyn gsb_core::shard::ShardLogic<
                                    World,
                                    GroupKey = (),
                                    State = gsb_game::sharded::ShardedRoomState,
                                >,
                            >,
                    )
                })
                .collect();
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |conn| {
                let (x, y) = gsb_game::room::spawn_pos(conn, spawn_half);
                gsb_game::sharded::shard_at(x, y, spawn_half, shard_count)
            }),
        }
    })
}

/// Start the server. Must be called from inside a tokio runtime. Metric
/// reports go to the tracing logger (one `gsb-metric` line per scope per
/// second; visible under `RUST_LOG=info`, silent without a subscriber).
pub async fn start_server(cfg: Config) -> Result<ServerHandle, ServerError> {
    start_inner(cfg, MetricSink::Log).await
}

/// Start the server with a programmatic metrics consumer: each report is
/// sent to `report_tx` (see [`gsb_core::metrics`]). Used by the load
/// generator and by tests that assert on server-side counters.
pub async fn start_server_metrics(
    cfg: Config,
    report_tx: mpsc::UnboundedSender<MetricReport>,
) -> Result<ServerHandle, ServerError> {
    start_inner(cfg, MetricSink::Channel(report_tx)).await
}

async fn start_inner(cfg: Config, metric_sink: MetricSink) -> Result<ServerHandle, ServerError> {
    let bind: SocketAddr = cfg.bind.parse().map_err(|e: std::net::AddrParseError| {
        ServerError::BadBind(cfg.bind.clone(), e.to_string())
    })?;

    // The sharded topology is a grid of 1..=16 shards (see
    // `gsb_game::sharded::grid_shape`); a count outside that range would
    // build a degenerate (or impossible) grid, so refuse to start.
    if cfg.visibility == Visibility::Sharded
        && !(1..=16).contains(&cfg.shard_count)
    {
        return Err(ServerError::BadShardCount(cfg.shard_count));
    }

    let table = build_table();
    let (reg_tx, reg_rx) = channel::<RegistryMsg>(4096);

    // The global ticker: one broadcast channel + one timing task. Rooms
    // subscribe to it at creation; aborting the task closes the broadcast,
    // which is the rooms' global stop signal (in addition to the control
    // Shutdown they receive during registry teardown). The metrics
    // collector subscribes to the same broadcast as its clock (see
    // `gsb_core::metrics` for the design).
    let (ticker, ticker_task) = gsb_core::ticker::Ticker::spawn(cfg.tick_hz, 64);
    // A3: the metrics event channel is *bounded* (DESIGN §2: bounded capacity
    // is the backpressure mechanism) and producers send with the synchronous
    // `try_send` (a drop is counted, harmless — the counters are cumulative).
    // Capacity 4096 ≈ the worst burst: N startup `ConnOpened` registry samples
    // + N shutdown final-flush connection samples (2N ≈ 2000 at 1000 conns),
    // with ~60× headroom over the collector's steady-state occupancy (it drains
    // the whole channel every tick; a tick holds only ~tens of samples). A drop
    // could still happen under a pathological stall — it is counted and
    // reported, never a stall.
    let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(4096);
    let metrics = tokio::spawn(
        MetricsCollector::new(
            ticker.subscribe(),
            metrics_rx,
            metric_sink,
            std::time::Duration::from_secs(1),
        )
        .run(),
    );

    // The registry runs until Shutdown; dropping the handle is fine. It
    // keeps a clone of its own mailbox so dispatcher tasks can report back.
    // The factory (and hence the registry's group-key type) is chosen at
    // this config boundary from the visibility strategy: each strategy is
    // a different `RoomLogic` group key (`()`, `Cell`, `Team`, `Sector`),
    // so the four arms are otherwise identical and each yields a
    // `JoinHandle<()>`.
    let _registry = match cfg.visibility {
        Visibility::All => {
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    demo_room_factory(cfg.spawn_half_size),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                )
                .run(),
            )
        }
        Visibility::Spatial => {
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    aoi_room_factory(cfg.aoi_cell_size, cfg.spawn_half_size),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                )
                .run(),
            )
        }
        Visibility::Team => {
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    team_room_factory(cfg.team_vision_radius, cfg.spawn_half_size),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                )
                .run(),
            )
        }
        Visibility::Pvs => {
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    pvs_room_factory(cfg.spawn_half_size),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                )
                .run(),
            )
        }
        Visibility::Sharded => {
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    sharded_room_factory(cfg.spawn_half_size, cfg.shard_count as usize),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                )
                .run(),
            )
        }
    };

    // Pre-create rooms 1..=room_count (all at the global rate; a room may
    // configure a slower rate that divides it).
    for id in 1..=cfg.room_count {
        let config = RoomConfig {
            id: RoomId(id),
            tick_hz: cfg.tick_hz,
            control_capacity: cfg.room_control,
            action_capacity: cfg.conn_action,
            max_snapshot_bytes: cfg.max_snapshot_bytes,
            keepalive_hz: cfg.keepalive_hz,
            max_players: cfg.max_players.map(|n| n as usize),
            ..Default::default()
        };
        {
            let tx = reg_tx.clone();
            tokio::spawn(async move {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(RegistryMsg::CreateRoom {
                        config,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                match reply_rx.await {
                    Ok(Ok(room)) => info!(room = %room, "room created"),
                    Ok(Err(e)) => warn!(error = %e, "room creation failed"),
                    Err(_) => warn!("registry gone before room reply"),
                }
            });
        }
    }

    // Session-lifecycle idle window (`0` disables): the reader pump's
    // clock on TCP, the demux deadline heap's window on rUDP.
    let idle_timeout = (cfg.idle_timeout_secs > 0.0).then(|| {
        std::time::Duration::from_secs_f64(cfg.idle_timeout_secs)
    });

    // The rUDP cookie key: the operator's 32-hex-char config string, or
    // `None` = the transport draws 16 bytes from the OS entropy source
    // at bind time. A parse failure is a config error (the operator sees
    // it at startup, before anything binds); an entropy failure is a
    // bind error (the server refuses to start with a predictable key —
    // see `gsb_net::udp::CookieKey`).
    let cookie_key = if cfg.transport == TransportKind::Udp {
        cfg.udp_cookie_key
            .as_deref()
            .map(parse_cookie_key)
            .transpose()
            .map_err(ServerError::BadCookieKey)?
    } else {
        None
    };

    // Bind the transport (config-selectable: TCP or rUDP — same actor
    // layer, see `TransportKind`), then run the accept loop.
    let transport: Arc<dyn Transport> = match cfg.transport {
        TransportKind::Tcp => Arc::new(TcpTransport {
            max_frame_bytes: cfg.max_frame_bytes,
        }),
        TransportKind::Udp => Arc::new(UdpTransport {
            config: UdpTransportConfig {
                // The demux pre-creates the mailboxes at handshake: same
                // capacities as the TCP path (cfg.conn_inbox/conn_out are
                // the fallbacks `Endpoint::take_*` would use).
                inbox_capacity: cfg.conn_inbox,
                outbox_capacity: cfg.conn_out,
                max_datagram_bytes: cfg.udp_max_datagram_bytes,
                idle_timeout,
                cookie_key,
            },
        }),
    };
    let listener = transport.bind(bind).await?;
    let addr = listener.local_addr().ok_or_else(|| {
        ServerError::BadBind(cfg.bind.clone(), "listener reports no address".into())
    })?;

    let accept_tx = reg_tx.clone();
    let conn_metrics_tx = metrics_tx.clone();
    let accept_listener = Arc::clone(&listener);
    let accept = tokio::spawn(async move {
        info!(%addr, "accepting connections");
        let mut next_conn: u64 = 1;
        loop {
            // `accept` consumes the Arc; clone it per iteration.
            let l = Arc::clone(&accept_listener);
            let mut endpoint = match l.accept().await {
                Ok(endpoint) => endpoint,
                Err(e) => {
                    warn!(%e, "accept error; backing off");
                    // Back off: a persistent error (e.g. EMFILE) must not
                    // turn this loop into a CPU-burning spin.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };

            let conn = ConnectionId(next_conn);
            next_conn += 1;

            // The peer address (for the connection actor's violation-close
            // signal — see `gsb_core::conn`); a transport that does not
            // expose one reports an unspecified address.
            let peer = endpoint
                .peer()
                .unwrap_or_else(|| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));

            // The connection's mailboxes, from the endpoint: pre-created
            // by a transport that establishes the session itself (rUDP:
            // at handshake, before this loop runs), created here for a
            // transport that does not (TCP — the exact channels this loop
            // used to create directly).
            let (in_tx, in_rx) = endpoint.take_inbox(cfg.conn_inbox);
            let (out_tx, out_rx) = endpoint.take_outbox(cfg.conn_out);

            // Reader + writer pumps (they finish on their own when the
            // peer or the actor goes away; the idle window, if enabled,
            // is what detects a half-open peer that sends nothing).
            let _pumps = endpoint.start_pump(conn, in_tx.clone(), out_rx, idle_timeout);

            // Register before spawning the actor: the registry owns the
            // notification path, and it must know the inbox before any
            // frame can reach the actor.
            let _ = accept_tx
                .send(RegistryMsg::ConnOpened {
                    conn,
                    inbox: in_tx.clone(),
                })
                .await;

            // One cheap sender clone per connection (unbounded sender is
            // an Arc).
            tokio::spawn(
                ConnectionActor::new(
                    conn,
                    peer,
                    Arc::clone(&table),
                    accept_tx.clone(),
                    in_rx,
                    out_tx,
                    conn_metrics_tx.clone(),
                )
                .run(),
            );
        }
    });

    Ok(ServerHandle {
        registry: reg_tx,
        accept,
        ticker: ticker_task,
        metrics,
        listener,
        addr,
    })
}
