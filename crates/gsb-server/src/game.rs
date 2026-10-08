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

use gsb_core::calls::{
    AnyOutcome, BoxError, Dispatch, Served, ServiceClient, ServiceDef, Services, Setup,
};
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
    /// service handed to [`RegistryParts::service`] (a `#[service]`
    /// implementation) or [`RegistryParts::add_service`] (a hand-written
    /// one) first is stopped explicitly by `ServerHandle::stop`, after
    /// the rooms.
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

    /// The top-level config keys this game owns (BACKLOG F62). A name
    /// owns the key of that name at the top of the file, whatever it
    /// holds: a flat value (`speed = 3`) or a table (`[<name>]`,
    /// `[<name>.<sub>]`, `[[<name>]]`). The server refuses, before
    /// anything binds, every top-level key that is neither the engine's
    /// nor owned by this game or by another game compiled into the build
    /// (a file may carry a sibling game's table); a table for a game the
    /// build does not host is refused too.
    ///
    /// Default: the table named after the game (`[<name>]`, the
    /// convention of GAME-MODULE §6 decision 1). A game that reads flat
    /// keys from `raw` declares them here (the 2D demo declares its flat
    /// keys and no table); one that reads nothing may return none. Read
    /// before [`Self::configure`], on a fresh module too (the server asks
    /// every compiled-in game), so it must not depend on the settings.
    fn owned_keys(&self) -> Vec<&'static str> {
        vec![self.name()]
    }

    /// What the input-idle ceiling does to the member it expires, by
    /// default, in every hosted room (BACKLOG E6; `RoomConfig::afk_action`)
    /// unless the operator writes `afk_action` (flat, or in a
    /// `[rooms.<id>]`). Whether an AFK player is kicked from the room or
    /// from the server is the game's (or the deployment's) call; the
    /// default here is `LeaveRoom` — today's behaviour, the socket stays
    /// open. It acts only where the operator sets the ceiling itself
    /// (`max_idle_input_secs`). Read once, after [`Self::configure`].
    fn afk_action(&self) -> gsb_core::room::AfkAction {
        gsb_core::room::AfkAction::LeaveRoom
    }
}

/// Everything `Registry::new` takes except the room factory: built by the
/// server, consumed by the module's [`GameModule::spawn_registry`]. The
/// fields stay private — a module can only hand them to
/// [`RegistryParts::spawn`], unchanged — plus the game services the
/// module registers on the way ([`RegistryParts::service`],
/// [`RegistryParts::add_service`]).
pub struct RegistryParts {
    pub(crate) inbox: Inbox<RegistryMsg>,
    pub(crate) self_mailbox: Mailbox<RegistryMsg>,
    pub(crate) ticker: Ticker,
    pub(crate) metrics: mpsc::Sender<MetricsEvent>,
    pub(crate) max_connections: Option<u64>,
    pub(crate) max_unauth_conns: Option<u64>,
    /// One source's unauthenticated connections (D12; `None` = no cap).
    pub(crate) max_unauth_per_source: Option<u64>,
    pub(crate) result_sink: Option<Mailbox<MatchResult>>,
    /// The rooms' drop barrier token (the server keeps the waiter).
    pub(crate) rooms_hold: Hold,
    /// The services registered so far, in registration order (see
    /// [`Self::service`] and [`Self::add_service`]).
    pub(crate) services: Vec<Service>,
    /// The `#[service]` implementations hosted on tower (their routes,
    /// for [`Self::client`]); their stops go into `services`.
    pub(crate) hosted: Services,
    /// The publishing handles minted so far (`docs/EVENTS.md` §5.1; see
    /// [`Self::publisher`]).
    pub(crate) publishers: gsb_core::events::Publishers,
    /// The engine-event subscribers registered so far (`docs/EVENTS.md`
    /// §5.4; see [`Self::engine_events`] and [`Self::service`]).
    pub(crate) engine: gsb_core::events::EngineSubscribers,
    /// What the registry records (`[record]`, feature `record`; B170).
    #[cfg(feature = "record")]
    pub(crate) record: Option<gsb_core::registry::RecordPlan>,
}

impl RegistryParts {
    /// Host a `#[gsb_core::service]` implementation on tower
    /// (`docs/SERVICES.md` §4.1): spawn it with the stack `setup` builds
    /// (`|s| s.timeout(..).concurrency_limit(..).load_shed()`; a
    /// [`DEFAULT_SERVICE_TIMEOUT`](gsb_core::calls::DEFAULT_SERVICE_TIMEOUT)
    /// deadline unless the setup sets its own), and stop it like an
    /// [`Self::add_service`] one — after the rooms (F5). Its clients come
    /// from [`Self::client`].
    ///
    /// ```ignore
    /// parts.service(Inventory::serve(PgInventory::new(db)), |s| s.timeout(Duration::from_secs(3)))?;
    /// let inv: InventoryClient = parts.client();   // rooms (non-blocking)
    /// let inv_h: InventoryHandle = parts.client(); // services, admin (awaitable)
    /// ```
    ///
    /// # Errors
    ///
    /// [`RegisterError::Duplicate`](gsb_core::calls::RegisterError) when a
    /// service of the same kind is already hosted (BACKLOG B169: an error
    /// the module handles, not a panic); nothing is spawned for it.
    pub fn service<K, S, L, F>(
        &mut self,
        served: Served<K, S>,
        setup: F,
    ) -> Result<&mut Self, gsb_core::calls::RegisterError>
    where
        K: ServiceDef<S>,
        S: Send + Sync + 'static,
        F: FnOnce(Setup<tower::layer::util::Identity>) -> Setup<L>,
        L: tower::Layer<K::Router>,
        L::Service:
            tower::Service<Dispatch<K::Bare>, Response = AnyOutcome> + Clone + Send + 'static,
        <L::Service as tower::Service<Dispatch<K::Bare>>>::Future: Send,
        <L::Service as tower::Service<Dispatch<K::Bare>>>::Error: Into<BoxError> + Send,
    {
        self.hosted.service(served, setup)?;
        self.services.extend(self.hosted.take_stops());
        for s in self.hosted.take_engine_subscriptions() {
            self.engine.add(s.name, s.interest, s.tx);
        }
        Ok(self)
    }

    /// A generated client of a hosted service (`InventoryClient` for
    /// rooms, `InventoryHandle` for services and admin code).
    ///
    /// # Panics
    ///
    /// When the service is not hosted ([`Self::service`] first).
    pub fn client<C: ServiceClient>(&self) -> C {
        self.hosted.client()
    }

    /// [`Self::client`], `None` when the service is not hosted.
    pub fn try_client<C: ServiceClient>(&self) -> Option<C> {
        self.hosted.try_client()
    }

    /// A publishing handle named `name` (`docs/EVENTS.md` §5.1): the right
    /// to push events — to an identity, many, a room or everyone — into
    /// the registry's router. Hand it to a service's constructor, a room
    /// factory, an admin task. `name` is its metric label
    /// (`gsb_publisher_*{publisher="<name>"}`, the log's
    /// `scope=publisher`); the same name hands back the same handle (one
    /// id, one set of counters). Its body bound is the config's
    /// `max_event_bytes`.
    ///
    /// ```ignore
    /// let bank = parts.publisher("bank");
    /// parts.service(Bank::serve(PgBank::new(db, bank)), |s| s)?;
    /// ```
    pub fn publisher(&mut self, name: &str) -> gsb_core::events::Publisher {
        self.publishers.publisher(name)
    }

    /// Subscribe a plain receiver to the engine's events
    /// (`docs/EVENTS.md` §5.4): `interest`'s [`EngineEvent`]s — an
    /// identity online, offline, in or out of a room, parked, resumed —
    /// numbered per subscriber, into a channel of `cap`. The registry
    /// never waits for it: a full or gone receiver loses the event,
    /// counted under `name` (`gsb_engine_events_*_total{subscriber}`).
    /// A `#[gsb_core::service]` with a `#[gsb(engine_events)]` method
    /// subscribes through [`Self::service`]'s setup instead
    /// (`|s| s.engine_events(Interest::PRESENCE)`).
    ///
    /// ```ignore
    /// let mut rx = parts.engine_events("presence-audit", Interest::ONLINE | Interest::OFFLINE, 1024);
    /// tokio::spawn(async move { while let Some(n) = rx.recv().await { audit(n).await } });
    /// ```
    ///
    /// [`EngineEvent`]: gsb_core::events::EngineEvent
    pub fn engine_events(
        &mut self,
        name: &str,
        interest: gsb_core::events::Interest,
        cap: usize,
    ) -> gsb_core::events::EngineReceiver {
        self.engine.subscribe(name, interest, cap)
    }

    /// The presence query handle (`docs/EVENTS.md` §20 E12): who is
    /// online now, paged, answered by the registry from its own table —
    /// what a restarted service (or one that saw a gap in its engine
    /// events) re-reads when it first needs it.
    pub fn presence(&self) -> gsb_core::events::Presence {
        gsb_core::events::Presence::new(self.self_mailbox.clone())
    }

    /// Register a hand-written game service for an explicit stop
    /// (BACKLOG F5, DESIGN §9.2): `ServerHandle::stop` asks it to stop
    /// only after every room and shard task has ended (their
    /// `on_shutdown` and `match_result` ran, so what a room sent it on
    /// the way out is already queued), then waits for it up to a
    /// deadline and aborts it past that. Opt-in: a service never
    /// registered keeps its own life (it ends when its last sender
    /// drops). (Before round q2 this was `service`; a `#[service]`
    /// implementation goes through [`Self::service`].)
    pub fn add_service(&mut self, service: Service) {
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
        let registry = Registry::new(
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
        .with_unauth_per_source(self.max_unauth_per_source);
        let engine_stats = self.engine.sources();
        let registry = registry.with_engine_events(self.engine);
        // The input logs `[record]` asks for (B170).
        #[cfg(feature = "record")]
        let registry = match self.record {
            Some(plan) => registry.with_record(plan),
            None => registry,
        };
        let task = tokio::spawn(registry.run());
        RegistryTask {
            _task: task,
            services: self.services,
            service_stats: self.hosted.stats().to_vec(),
            publisher_stats: self.publishers.sources(),
            engine_stats,
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
    /// The hosted services' stats, for the metrics collector (B164).
    pub(crate) service_stats: Vec<(&'static str, std::sync::Arc<gsb_core::calls::ServiceStats>)>,
    /// The publishing handles the module minted, for the metrics
    /// collector (`docs/EVENTS.md` §9).
    pub(crate) publisher_stats: Vec<gsb_core::events::PublisherSource>,
    /// The engine-event subscribers, for the metrics collector
    /// (`docs/EVENTS.md` §5.4).
    pub(crate) engine_stats: Vec<gsb_core::events::EngineSubscriberSource>,
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
