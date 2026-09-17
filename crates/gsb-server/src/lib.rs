//! gsb composition root.
//!
//! [`start_server`] wires the whole stack together: the transport (default
//! TCP, pluggable), the registry actor (control plane), the pre-created
//! rooms (via the game crate's [`gsb_game::room::OpenRoom`]), and the accept
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

pub(crate) mod boot;
pub(crate) mod config;
mod http;

pub use boot::{
    ServerHandle, ServerHooks, build_table, start_server, start_server_metrics,
    start_server_metrics_with, start_server_with,
};
pub use config::{
    Communication, Config, ConfigError, ListenerEntry, ListenerTransport, ResolvedSelection,
    RoomKind, ServerError, Topology, TransportKind, Visibility, VisibilityAxis,
};

use crate::config::*;
