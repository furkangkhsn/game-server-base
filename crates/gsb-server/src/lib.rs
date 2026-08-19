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

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::{MetricReport, MetricSink, MetricsCollector, MetricsEvent};
use gsb_core::registry::{Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{RoomConfig, RoomLogic};
use gsb_net::tcp::TcpTransport;
use gsb_net::transport::Transport;
use gsb_protocol::MessageTable;

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
    /// Warn when a room group's snapshot payload exceeds this many bytes
    /// (rUDP MTU readiness; default = `max_frame_bytes`).
    pub max_snapshot_bytes: usize,
    /// Keep-alive rate for unchanged snapshot groups, in Hz (a client that
    /// lost its last snapshot must not stay stale forever). `<= 0` disables.
    pub keepalive_hz: f64,
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
            max_snapshot_bytes: gsb_net::tcp::DEFAULT_MAX_FRAME_BYTES,
            keepalive_hz: 1.0,
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
        let cfg: Self = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.display().to_string(),
            source: e,
        })?;
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
}

/// Handle to a running server.
pub struct ServerHandle {
    registry: gsb_core::channel::Mailbox<RegistryMsg>,
    accept: JoinHandle<()>,
    ticker: JoinHandle<()>,
    /// The metrics collector (emits one final report when the ticker's
    /// broadcast closes).
    metrics: JoinHandle<()>,
    /// The actual bound address (useful when binding port 0 in tests).
    pub addr: SocketAddr,
}

impl ServerHandle {
    /// Shut the server down: the registry tears down connections and rooms
    /// (rooms get a control `Shutdown`, processed on their next tick); the
    /// ticker is aborted, which closes the broadcast and stops any room that
    /// missed its window; the accept loop is hard-aborted (documented v1
    /// limitation). The metrics collector is awaited last: it emits one
    /// final report when the broadcast closes.
    pub async fn stop(self) {
        let _ = self.registry.send(RegistryMsg::Shutdown).await;
        self.ticker.abort();
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
/// [`gsb_game::room::DemoRoom`]. Group key is `()` (one group per room).
fn demo_room_factory() -> RoomFactory<World, ()> {
    Arc::new(|_id, _config| {
        (
            World::new(),
            Box::new(gsb_game::room::DemoRoom::default())
                as Box<dyn RoomLogic<World, GroupKey = ()>>,
        )
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

    let table = build_table();
    let (reg_tx, reg_rx) = channel::<RegistryMsg>(4096);

    // The global ticker: one broadcast channel + one timing task. Rooms
    // subscribe to it at creation; aborting the task closes the broadcast,
    // which is the rooms' global stop signal (in addition to the control
    // Shutdown they receive during registry teardown). The metrics
    // collector subscribes to the same broadcast as its clock (see
    // `gsb_core::metrics` for the design).
    let (ticker, ticker_task) = gsb_core::ticker::Ticker::spawn(cfg.tick_hz, 64);
    let (metrics_tx, metrics_rx) = mpsc::unbounded_channel::<MetricsEvent>();
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
    let _registry = tokio::spawn(
        Registry::new(
            reg_rx,
            reg_tx.clone(),
            demo_room_factory(),
            ticker.clone(),
            metrics_tx.clone(),
        )
        .run(),
    );

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

    // Bind the transport, then run the accept loop.
    let transport: Arc<dyn Transport> = Arc::new(TcpTransport {
        max_frame_bytes: cfg.max_frame_bytes,
    });
    let listener = transport.bind(bind).await?;
    let addr = listener.local_addr().ok_or_else(|| {
        ServerError::BadBind(cfg.bind.clone(), "listener reports no address".into())
    })?;

    let accept_tx = reg_tx.clone();
    let conn_metrics_tx = metrics_tx.clone();
    let accept = tokio::spawn(async move {
        info!(%addr, "accepting connections");
        let mut next_conn: u64 = 1;
        loop {
            // `accept` consumes the Arc; clone it per iteration.
            let l = Arc::clone(&listener);
            let endpoint = match l.accept().await {
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

            let (in_tx, in_rx) = channel::<ConnIn>(cfg.conn_inbox);
            let (out_tx, out_rx) = channel::<FrameBatch>(cfg.conn_out);

            // Reader + writer pumps (they finish on their own when the
            // peer or the actor goes away).
            let _pumps = endpoint.start_pump(conn, in_tx.clone(), out_rx);

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
        addr,
    })
}
