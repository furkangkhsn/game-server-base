//! gsb composition root.
//!
//! [`start_server`] wires the whole stack together: the transport (default
//! TCP, pluggable), the registry actor (control plane), the pre-created
//! rooms (via the game crate's [`gsb_game::room::DemoRoom`]), and the accept
//! loop. It must be called from inside a tokio runtime.
//!
//! ```text
//! accept loop ──ConnOpened──▶ registry actor ◀──RegistryMsg── connection actors
//!     │                           │CreateRoom/SpawnPlayer/…
//!     ▼                           ▼
//! endpoint pumps             room actors (4-phase tick)
//! (reader/writer per conn)      │RoomMsg::Action / RoomMsg::Tick
//!                                ▼
//!                          World (bevy_ecs) + RoomLogic (gsb-game)
//! ```

use std::net::SocketAddr;
use std::sync::Arc;

use bevy_ecs::world::World;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::registry::{Registry, RegistryMsg, RoomFactory};
use gsb_core::room::RoomConfig;
use gsb_net::tcp::TcpTransport;
use gsb_net::transport::Transport;
use gsb_protocol::MessageTable;

/// Server configuration (see `config.example.toml`).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Socket address to bind (e.g. `"0.0.0.0:7777"`, `"127.0.0.1:0"`).
    pub bind: String,
    /// Simulation rate in ticks per second.
    pub tick_hz: f64,
    /// Number of rooms to pre-create at startup (ids `1..=room_count`).
    pub room_count: u64,
    /// Maximum frame body size in bytes (transport-level guard).
    pub max_frame_bytes: usize,
    /// Capacity of each room's mailbox.
    pub room_mailbox: usize,
    /// Capacity of each connection's inbound (frames in) mailbox.
    pub conn_inbox: usize,
    /// Capacity of each connection's outbound (batches out) channel.
    pub conn_out: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7777".into(),
            tick_hz: 30.0,
            room_count: 1,
            max_frame_bytes: gsb_net::tcp::DEFAULT_MAX_FRAME_BYTES,
            room_mailbox: 4096,
            conn_inbox: 1024,
            conn_out: 256,
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
    /// The actual bound address (useful when binding port 0 in tests).
    pub addr: SocketAddr,
}

impl ServerHandle {
    /// Shut the server down: the registry tears down connections and rooms;
    /// the accept loop is hard-aborted (documented v1 limitation).
    pub async fn stop(self) {
        let _ = self.registry.send(RegistryMsg::Shutdown).await;
        self.accept.abort();
    }
}

/// The demo composition: base protocol + demo game messages.
pub fn build_table() -> Arc<MessageTable> {
    let mut table = gsb_protocol::base_table();
    gsb_game::register(&mut table);
    Arc::new(table)
}

/// The room factory for the demo game: an empty bevy `World` + a
/// [`gsb_game::room::DemoRoom`].
fn demo_room_factory() -> RoomFactory<World> {
    Arc::new(|_id, _config| (World::new(), Box::new(gsb_game::room::DemoRoom::default())))
}

/// Start the server. Must be called from inside a tokio runtime.
pub async fn start_server(cfg: Config) -> Result<ServerHandle, ServerError> {
    let bind: SocketAddr = cfg.bind.parse().map_err(|e: std::net::AddrParseError| {
        ServerError::BadBind(cfg.bind.clone(), e.to_string())
    })?;

    let table = build_table();
    let (reg_tx, reg_rx) = channel::<RegistryMsg>(cfg.room_mailbox);

    // The registry runs until Shutdown; dropping the handle is fine.
    let _registry = tokio::spawn(Registry::new(reg_rx, demo_room_factory()).run());

    // Pre-create rooms 1..=room_count.
    for id in 1..=cfg.room_count {
        let config = RoomConfig {
            id: RoomId(id),
            tick_hz: cfg.tick_hz,
            mailbox_capacity: cfg.room_mailbox,
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
    let accept = tokio::spawn(async move {
        info!(%addr, "accepting connections");
        let mut next_conn: u64 = 1;
        loop {
            // `accept` consumes the Arc; clone it per iteration.
            let l = Arc::clone(&listener);
            let endpoint = match l.accept().await {
                Ok(endpoint) => endpoint,
                Err(e) => {
                    warn!(%e, "accept error; retrying");
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

            tokio::spawn(
                ConnectionActor::new(conn, Arc::clone(&table), accept_tx.clone(), in_rx, out_tx)
                    .run(),
            );
        }
    });

    Ok(ServerHandle {
        registry: reg_tx,
        accept,
        addr,
    })
}
