//! The 2D demo (`gsb-demo`) as a game module: its six room builds, the
//! three-axis selection that picks one, and the economy service its rooms
//! delegate `ECONOMY` requests to.
//!
//! The demo reads its settings from the flat `Config` keys every
//! pre-module config already uses (`visibility`, `topology`,
//! `communication`, `shard_count`, `aoi_cell_size`, `team_vision_radius`,
//! `spawn_half_size`, `disconnect_grace_secs`) — GAME-MODULE §6 decision 1.

use std::time::Duration;

use gsb_protocol::MessageTable;

use crate::config::Topology;
use crate::{Config, GameModule, RegistryParts, RegistryTask, ServerError};

mod axes;
mod factories;
mod select;

pub use axes::{ResolvedSelection, RoomKind, VisibilityAxis};
pub use factories::build_table;
use factories::*;
pub use select::resolve_selection;

/// What [`GameModule::configure`] settled: the resolved selection plus
/// the demo knobs the factories close over.
#[derive(Debug, Clone, Copy)]
struct Settings {
    selection: ResolvedSelection,
    spawn_half: f32,
    cell_size: f32,
    vision_radius: f32,
    shard_count: usize,
    disconnect_grace: Duration,
}

/// The 2D demo module. Unconfigured until the server calls
/// [`GameModule::configure`].
#[derive(Debug, Default)]
pub struct DemoModule {
    settings: Option<Settings>,
}

impl DemoModule {
    /// The `game` config key's value for this module.
    pub const NAME: &'static str = "demo";

    /// A fresh, unconfigured module.
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh module behind the trait (the catalog's constructor).
    pub(crate) fn boxed() -> Box<dyn GameModule> {
        Box::new(Self::new())
    }

    fn settings(&self) -> &Settings {
        self.settings
            .as_ref()
            .expect("the server configures a module before using it")
    }
}

/// The config's grace as a `Duration`, clamped at zero: a negative value
/// would panic `from_secs_f64`, and "negative grace" can only mean
/// "disabled" anyway.
fn grace_of(cfg: &Config) -> Duration {
    Duration::from_secs_f64(cfg.disconnect_grace_secs.max(0.0))
}

impl GameModule for DemoModule {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    /// The three-axis selection (topology × visibility × communication):
    /// derive the axes from the legacy spellings, honor explicit keys, and
    /// validate the combination — the server calls this BEFORE anything
    /// binds, so an unsupported combination fails cleanly at startup
    /// naming its roadmap phase.
    fn configure(&mut self, _raw: &toml::Table, engine: &Config) -> Result<(), ServerError> {
        let selection = resolve_selection(engine)?;
        // The sharded topology is a grid of 1..=256 shards (see
        // `gsb_demo::sharded::grid_shape`); a count outside that range would
        // build a degenerate (or impossible) grid, so refuse to start. Gated
        // on the RESOLVED topology: both the legacy spelling AND an explicit
        // `topology = "sharded"` take this path.
        if selection.topology == Topology::Sharded && !(1..=256).contains(&engine.shard_count) {
            return Err(ServerError::BadShardCount(engine.shard_count));
        }
        self.settings = Some(Settings {
            selection,
            spawn_half: engine.spawn_half_size,
            cell_size: engine.aoi_cell_size,
            vision_radius: engine.team_vision_radius,
            shard_count: engine.shard_count as usize,
            disconnect_grace: grace_of(engine),
        });
        Ok(())
    }

    fn register(&self, table: &mut MessageTable) {
        gsb_demo::register(table);
    }

    /// The factory (and hence the registry's group-key type) is chosen
    /// from the RESOLVED three-axis selection — never the raw legacy
    /// string: each room kind is a different `RoomLogic` group key (`()`,
    /// `Cell`, `Team`, `Sector`) or the sharded grid topology, so each arm
    /// spawns its own `Registry<W, G, St, Sp>`. `configure` already
    /// validated the combination; every arm here is a supported one.
    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let s = *self.settings();
        // One economy service per server (the RPC pattern's external-I/O
        // reference adapter), shared by clone with every room of EVERY
        // build: each kit room forwards requests to the game, so the
        // service is what decides whether `ECONOMY` is answered —
        // uniformly, whatever the axes resolved to (GAME-MODULE §6
        // decision 11).
        let economy = gsb_demo::economy::EconomyService::spawn(
            gsb_demo::economy::EconomyService::default_latency(),
        );
        let grace = s.disconnect_grace;
        // The sharded builds report one match result per shard through the
        // shared sink, under the logical room id (see `gsb_core::shard`).
        match s.selection.kind {
            RoomKind::Open => parts.spawn(open_room_factory(s.spawn_half, grace, economy)),
            RoomKind::Aoi => {
                parts.spawn(aoi_room_factory(s.cell_size, s.spawn_half, grace, economy))
            }
            RoomKind::Team => parts.spawn(team_room_factory(
                s.vision_radius,
                s.spawn_half,
                grace,
                economy,
            )),
            RoomKind::Sector => parts.spawn(pvs_room_factory(s.spawn_half, grace, economy)),
            RoomKind::Sharded => parts.spawn(sharded_room_factory(
                s.spawn_half,
                s.shard_count,
                grace,
                economy,
            )),
            RoomKind::ShardedSpatial => parts.spawn(sharded_spatial_room_factory(
                s.spawn_half,
                s.shard_count,
                s.cell_size,
                grace,
                economy,
            )),
        }
    }

    fn describe(&self) -> String {
        match &self.settings {
            None => "demo (unconfigured)".into(),
            Some(s) => format!(
                "demo: topology={} visibility={} communication={} room={:?}",
                s.selection.topology,
                s.selection.visibility,
                s.selection.communication,
                s.selection.kind
            ),
        }
    }
}
