//! The composition root: what the server is made of, wired once at
//! startup and never again.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::task::JoinHandle;

use gsb_core::auth::TicketAuth;
use gsb_core::channel::{Inbox, Mailbox};
use gsb_core::error::CoreError;
use gsb_core::id::RoomId;
use gsb_core::registry::{MatchResult, RegistryMsg, RoomStatus};
use gsb_core::room::RoomConfig;

mod accept;
mod start;
mod stop;

pub use start::{
    start_game_server, start_game_server_with, start_server, start_server_metrics,
    start_server_metrics_with, start_server_with,
};
pub use stop::StopReport;

/// The composition-root's platform hooks (the base ships the wiring, the
/// platform ships the behaviour):
///
/// - [`Self::ticket`]: the ticket-validation hook (`gsb_core::auth`) —
///   `None` (the default) = the legacy local-auth path, byte-for-byte
///   unchanged. The base defines the hook and deliberately implements
///   NO validator (a signature-service call, a cache, … is platform
///   specific — the same adapter split as the transport).
#[derive(Clone, Default)]
pub struct ServerHooks {
    /// The ticket-validation hook for the connection actors; `None` =
    /// local auth (`Auth.name` accepted as-is).
    pub ticket: Option<TicketAuth>,
}

// ── the control-plane round trips (shared) ─────────────────────────────
//
// The three room-lifecycle round trips below are the WHOLE of what both
// consumers need — `ServerHandle`'s programmatic API and the HTTP ops
// surface (`http.rs`) — so they live once, over a bare registry mailbox.
// WHY not on `ServerHandle`: the HTTP tasks hold only the mailbox (a cheap
// clone), not the join handles/listener a full handle carries.

/// Idempotent create (see [`RegistryMsg::CreateRoom`]); the reply doubles
/// as a status query.
pub(crate) async fn registry_open_room(
    registry: &Mailbox<RegistryMsg>,
    config: RoomConfig,
) -> Result<RoomStatus, CoreError> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<RoomStatus, CoreError>>();
    registry
        .send(RegistryMsg::CreateRoom { config, reply: tx })
        .await
        .map_err(|_| CoreError::Io("registry gone".into()))?;
    rx.await
        .map_err(|_| CoreError::Io("registry dropped the reply".into()))?
}

/// Idempotent destroy (see [`RegistryMsg::DestroyRoom`]).
pub(crate) async fn registry_close_room(
    registry: &Mailbox<RegistryMsg>,
    id: RoomId,
) -> Result<RoomStatus, CoreError> {
    let (tx, rx) = tokio::sync::oneshot::channel::<RoomStatus>();
    registry
        .send(RegistryMsg::DestroyRoom { id, reply: tx })
        .await
        .map_err(|_| CoreError::Io("registry gone".into()))?;
    rx.await
        .map_err(|_| CoreError::Io("registry dropped the reply".into()))
}

/// Table-only status query (see [`RegistryMsg::RoomStatus`]).
pub(crate) async fn registry_room_status(
    registry: &Mailbox<RegistryMsg>,
    id: RoomId,
) -> Result<RoomStatus, CoreError> {
    let (tx, rx) = tokio::sync::oneshot::channel::<RoomStatus>();
    registry
        .send(RegistryMsg::RoomStatus { id, reply: tx })
        .await
        .map_err(|_| CoreError::Io("registry gone".into()))?;
    rx.await
        .map_err(|_| CoreError::Io("registry dropped the reply".into()))
}

/// Handle to a running server.
pub struct ServerHandle {
    registry: Mailbox<RegistryMsg>,
    /// The room template this server builds its rooms from — the config
    /// file's room-level keys over the hosted game's defaults (see
    /// [`Self::room_config`]).
    rooms: crate::config::RoomTemplate,
    /// One accept task PER listener (all sharing the pipeline below and
    /// one connection-id sequence). Each ends when `stop` closes its
    /// listener (abort is only the backstop — see `stop`).
    accepts: Vec<JoinHandle<()>>,
    ticker: JoinHandle<()>,
    /// The metrics collector: its final report goes out once the ticker's
    /// broadcast has closed AND every session producer has dropped its
    /// sender (bounded, F35); `true` = that report is complete.
    metrics: JoinHandle<bool>,
    /// The game's registered services (`RegistryParts::service`), stopped
    /// by `stop` after the rooms (BACKLOG F5).
    services: Vec<gsb_core::service::Service>,
    /// The rooms' drop barrier: released once the registry and every room
    /// and shard task have ended (their teardown hooks ran).
    rooms_released: gsb_core::service::Released,
    /// The HTTP ops surface, when `http_listen` was configured. `stop`
    /// closes its door, which ends its accept loop (the listener drops
    /// with it); abort is only the backstop, as for the listeners (B33).
    http: Option<crate::http::OpsSurface>,
    /// The bound listeners, in config order. `stop` closes each, which
    /// ends its accept task (the pending `accept` returns the
    /// listener-closed error) and, for the rUDP transport, stops the
    /// shared demux task (a plain drop would not reach it — see
    /// `Listener::close`).
    listeners: Vec<Arc<dyn gsb_net::transport::Listener>>,
    /// The actual bound address of the FIRST listener (useful when binding
    /// port 0 in tests; kept for the legacy single-listener callers).
    pub addr: SocketAddr,
    /// The actual bound address of EVERY listener, in config order (the
    /// multi-listener shape; `addr` is `addrs[0]`).
    pub addrs: Vec<SocketAddr>,
    /// The actual bound address of the HTTP ops surface; `None` when the
    /// surface is disabled (`http_listen` empty — the default).
    pub http_addr: Option<SocketAddr>,
    /// The match-result sink (the control plane's result seam, see
    /// `gsb_core::room::RoomLogic::match_result`): the room's result
    /// arrives here on shutdown. The reference adapter is the
    /// composition root reading this receiver (one hop, in-process,
    /// bounded — no NATS/Kafka/gRPC in the base).
    pub match_results: Inbox<MatchResult>,
}

impl ServerHandle {
    /// Open a room at runtime (the control plane's lifecycle API, feature
    /// A). **Idempotent**: creating a room that already exists with the
    /// IDENTICAL `config` is a no-op that reports the existing room's
    /// status (one room, not two — a resent request must not double the
    /// room); a different config for a live room is a
    /// [`CoreError::RoomConflict`]. The round trip doubles as a status
    /// query (the reply carries the [`RoomStatus`]).
    pub async fn open_room(&self, config: RoomConfig) -> Result<RoomStatus, CoreError> {
        registry_open_room(&self.registry, config).await
    }

    /// Room `id` as THIS server builds it: the boot rooms and the admin
    /// surface's opens come from the same template — the config's
    /// room-level keys and `[rooms.<id>]` over the hosted game's default
    /// input rate limit (`GameModule::input_rate`). Reopening a boot room
    /// with it is the idempotent no-op; `Config::room_config` is the
    /// file's view, without the game's default.
    pub fn room_config(&self, id: u64) -> RoomConfig {
        self.rooms.room(id)
    }

    /// Close a room at runtime (feature A). **Idempotent**: closing a room
    /// that is not running is a no-op reported as [`RoomStatus::Absent`]
    /// (a control plane that retries a close never sees an error for the
    /// second attempt). The room stops on its next tick; connections
    /// affiliated with it are notified (`RoomGone`).
    pub async fn close_room(&self, id: RoomId) -> Result<RoomStatus, CoreError> {
        registry_close_room(&self.registry, id).await
    }

    /// Query a room's status (feature A): [`RoomStatus::Running`] (with
    /// the member count), or [`RoomStatus::Absent`]. The registry answers
    /// from its own table (it never awaits a room).
    pub async fn room_status(&self, id: RoomId) -> Result<RoomStatus, CoreError> {
        registry_room_status(&self.registry, id).await
    }
}
