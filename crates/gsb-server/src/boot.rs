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
mod factories;
mod start;

pub use factories::build_table;
pub use start::{start_server, start_server_metrics, start_server_metrics_with, start_server_with};

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
    /// One accept task PER listener (all sharing the pipeline below and
    /// one connection-id sequence). All are aborted on stop.
    accepts: Vec<JoinHandle<()>>,
    ticker: JoinHandle<()>,
    /// The metrics collector (emits one final report when the ticker's
    /// broadcast closes).
    metrics: JoinHandle<()>,
    /// The HTTP ops-surface task, when `http_listen` was configured.
    /// Aborted on stop (its listener drops with the aborted future).
    http: Option<JoinHandle<()>>,
    /// The bound listeners, in config order. `stop` closes each *before*
    /// aborting its accept task: for the rUDP transport this is what stops
    /// the shared demux task (a plain drop would not reach it — see
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
    /// Shut the server down: the registry tears down connections and rooms
    /// (rooms get a control `Shutdown`, processed on their next tick; a
    /// room with a configured result seam reports it on that shutdown); the
    /// ticker is aborted, which closes the broadcast and stops any room that
    /// missed its window; EVERY listener is closed (stopping any per-
    /// listener transport shared state, e.g. each rUDP demux); every accept
    /// loop is hard-aborted (documented v1 limitation). The HTTP ops surface
    /// is aborted with it. The metrics collector is awaited last: it emits
    /// one final report when the broadcast closes. The teardown ORDER is the
    /// single-listener order applied across all listeners: doors close
    /// first, so no new client can connect while the registry is tearing
    /// the existing ones down.
    pub async fn stop(self) {
        let _ = self.registry.send(RegistryMsg::Shutdown).await;
        if let Some(http) = self.http {
            http.abort();
        }
        self.ticker.abort();
        for l in &self.listeners {
            l.close();
        }
        for accept in &self.accepts {
            accept.abort();
        }
        let _ = self.metrics.await;
    }

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
