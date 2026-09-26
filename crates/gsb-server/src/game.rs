//! The game-module seam (docs/GAME-MODULE.md §4.1): everything the
//! server must know to host a game built on the kit, behind one
//! object-safe trait.
//!
//! WHY the trait has no generic parameters: a room's types (`W`, `G`,
//! `St`, `Sp`) only matter to the registry actor that owns the rooms.
//! Everything outside that task holds a `Mailbox<RegistryMsg>`, which is
//! not generic. So the MODULE spawns the registry ([`RegistryParts::spawn`]
//! is the one generic method) and the server keeps a
//! `Box<dyn GameModule>` — no game type leaks into config or boot.

use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use gsb_core::channel::{Inbox, Mailbox};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{MatchResult, Registry, RegistryMsg, RoomFactory};
use gsb_core::service::{Hold, Service};
use gsb_core::ticker::Ticker;
use gsb_protocol::MessageTable;

use crate::{Config, ServerError};

/// A game the server can host. The server drives it in a fixed order,
/// once per start: [`Self::configure`] (before any socket binds, so a bad
/// game config never half-starts the server), [`Self::register`], then
/// [`Self::spawn_registry`]. After `configure` the module is only read.
pub trait GameModule: Send + Sync + 'static {
    /// The name the `game` config key selects; the loadgen's RESULT
    /// line carries it too (`game=<name>`).
    fn name(&self) -> &'static str;

    /// Read and validate the game's settings. `raw` is the parsed config
    /// file ([`Config::raw`]: empty for a config built in code), from
    /// which a module tells an EXPLICITLY written key from a defaulted
    /// one; `engine` is the typed engine config. A key the game fixes
    /// must be refused when written explicitly, never silently ignored
    /// (GAME-MODULE §6 decision 2).
    fn configure(&mut self, raw: &toml::Table, engine: &Config) -> Result<(), ServerError>;

    /// Add the game's messages to the base table.
    fn register(&self, table: &mut MessageTable);

    /// Spawn the registry actor (and the game's services) through
    /// [`RegistryParts::spawn`] — the only way to obtain the returned
    /// [`RegistryTask`], so a module cannot forget the registry. A
    /// service handed to [`RegistryParts::service`] first is stopped
    /// explicitly by `ServerHandle::stop`, after the rooms.
    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask;

    /// A one-line account of what was configured (the startup log).
    fn describe(&self) -> String;

    /// The game's default per-connection input rate limit (BACKLOG E1;
    /// docs/SECURITY.md "post-auth input volume"): every hosted room's
    /// `RoomConfig::input_rate` unless the operator writes
    /// `input_rate_hz` (flat, or in a `[rooms.<id>]`; `0` = off). The
    /// number is the game's — it knows its honest input cadence — so the
    /// default here is `None`: no limit, today's behaviour. Read once,
    /// after [`Self::configure`] (it may depend on the game's settings).
    fn input_rate(&self) -> Option<gsb_core::room::InputRate> {
        None
    }
}

/// Everything `Registry::new` takes except the room factory: built by the
/// server, consumed by the module's [`GameModule::spawn_registry`]. The
/// fields stay private — a module can only hand them to
/// [`RegistryParts::spawn`], unchanged — plus the game services the
/// module registers on the way ([`RegistryParts::service`]).
pub struct RegistryParts {
    pub(crate) inbox: Inbox<RegistryMsg>,
    pub(crate) self_mailbox: Mailbox<RegistryMsg>,
    pub(crate) ticker: Ticker,
    pub(crate) metrics: mpsc::Sender<MetricsEvent>,
    pub(crate) max_connections: Option<u64>,
    pub(crate) max_unauth_conns: Option<u64>,
    pub(crate) result_sink: Option<Mailbox<MatchResult>>,
    /// The rooms' drop barrier token (the server keeps the waiter).
    pub(crate) rooms_hold: Hold,
    /// The services registered so far (see [`Self::service`]).
    pub(crate) services: Vec<Service>,
}

impl RegistryParts {
    /// Register a game service for an explicit stop (BACKLOG F5,
    /// DESIGN §9.2): `ServerHandle::stop` asks it to stop only after every
    /// room and shard task has ended (their `on_shutdown` and
    /// `match_result` ran, so what a room sent it on the way out is
    /// already queued), then waits for it up to a deadline and aborts it
    /// past that. Opt-in: a service never registered keeps its own life
    /// (it ends when its last sender drops).
    pub fn service(&mut self, service: Service) {
        self.services.push(service);
    }

    /// Spawn the registry actor over `factory`. The generics live here
    /// and nowhere else; the bounds are exactly the registry's.
    pub fn spawn<W, G, St, Sp>(self, factory: RoomFactory<W, G, St, Sp>) -> RegistryTask
    where
        W: Send + 'static,
        G: Eq + Hash + Clone + Debug + Send + 'static,
        St: Debug + Send + 'static,
        Sp: Debug + Clone + PartialEq + Send + 'static,
    {
        let task = tokio::spawn(
            Registry::new(
                self.inbox,
                self.self_mailbox,
                factory,
                self.ticker,
                self.metrics,
                self.max_connections,
                self.max_unauth_conns,
                self.result_sink,
            )
            .with_rooms_hold(self.rooms_hold)
            .run(),
        );
        RegistryTask {
            _task: task,
            services: self.services,
        }
    }
}

/// The running registry actor, with the services registered before it.
/// Only [`RegistryParts::spawn`] makes one. The handle is held, never
/// awaited: the registry runs until its `Shutdown`, and dropping the
/// handle would merely detach it.
pub struct RegistryTask {
    _task: JoinHandle<()>,
    pub(crate) services: Vec<Service>,
}

/// Errors of game selection and of the game modules themselves
/// (carried by [`ServerError::Game`]).
#[derive(Debug, thiserror::Error)]
pub enum GameError {
    /// The `game` key names a game this build does not host.
    #[error("unknown game `{name}` (config key `game`): this build hosts {}", hosted(.compiled_in))]
    Unknown {
        /// The configured name.
        name: String,
        /// The games compiled into this build (cargo features).
        compiled_in: Vec<&'static str>,
    },

    /// The server was started with an explicit module, and the config
    /// file names a different game.
    #[error(
        "config key `game = \"{configured}\"` names a different game than the \
         module this server was started with (`{module}`)"
    )]
    NameMismatch {
        /// What the config file says.
        configured: String,
        /// The module's own name.
        module: &'static str,
    },

    /// A module's own error (e.g. a third-party game's config check).
    #[error("game `{game}`: {source}")]
    Module {
        /// The module's name.
        game: &'static str,
        /// What went wrong.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// The compiled-in list as the unknown-game message prints it.
fn hosted(games: &[&'static str]) -> String {
    if games.is_empty() {
        return "no games (built without any game feature)".into();
    }
    games
        .iter()
        .map(|g| format!("`{g}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
