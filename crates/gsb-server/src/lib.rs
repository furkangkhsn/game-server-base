//! gsb composition root.
//!
//! [`start_server`] wires the whole stack together: the transport (default
//! TCP, pluggable), the registry actor (control plane), the pre-created
//! rooms, and the accept loop. It must be called from inside a tokio
//! runtime. The game itself is a [`GameModule`] (docs/GAME-MODULE.md): the
//! config's `game` key picks one of the compiled-in [`games`] (default:
//! the 2D demo), or [`start_game_server`] hosts a module the caller built.
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
//!                                         World (bevy_ecs) + the game's rooms
//! ```

pub(crate) mod boot;
pub(crate) mod config;
mod error_chain;
mod game;
pub mod games;
mod http;

pub use boot::{
    ServerHandle, ServerHooks, StopReport, start_game_server, start_game_server_with, start_server,
    start_server_metrics, start_server_metrics_with, start_server_with,
};
pub use config::{
    Communication, Config, ConfigError, ListenerEntry, ListenerTransport, MetricsConfig,
    OtlpSection, RoomOverride, ServerError, Topology, TransportKind, Visibility,
};
pub use error_chain::error_chain;
pub use game::{GameError, GameModule, RegistryParts, RegistryTask};
// The 2D demo's selection types and wire table, at the paths they have
// always had (compatibility, GAME-MODULE §6 decision 1).
#[cfg(feature = "game-demo")]
pub use games::demo::{ResolvedSelection, RoomKind, VisibilityAxis, build_table};

use crate::config::*;
