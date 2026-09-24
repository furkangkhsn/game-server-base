//! The 3D team arena (`gsb-demo-arena`) as a game module: ONE strategy
//! — a single room with team fog of war over the kit's 3D vision preset,
//! full snapshots every tick (`TeamRoom<ArenaGame, VisionGrid3<Pos3>>`).
//!
//! Settings come from the `[arena]` table (GAME-MODULE §4.3):
//!
//! - `teams` — how many teams share a room (1..=255, default
//!   [`DEFAULT_TEAMS`] = 3); joiners are dealt round-robin;
//! - `disconnect_grace_secs` — how long a dropped player's unit is held
//!   (parked, resumable) before the arena's bot takes it back to its
//!   base (default the kit's 30 s; `0` = the unit leaves at once).
//!
//! Everything else the arena fixes, and an explicitly written flat key
//! for it refuses startup: the three axes, `shard_count`,
//! `aoi_cell_size`, `team_vision_radius` (the arena's own 15 m, 3D),
//! `spawn_half_size` (units spawn at their team's base), and the demo's
//! flat `disconnect_grace_secs` (the arena reads its own, above).

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::world::World;
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::RoomLogic;
use gsb_demo_arena::game::DEFAULT_TEAMS;
use gsb_demo_arena::{ArenaGame, VISION_RADIUS, arena_room};
use gsb_kit::team::Team;
use gsb_protocol::MessageTable;

use super::settings::{self, FixedKey, secs_label};
use crate::{Config, GameModule, RegistryParts, RegistryTask, ServerError};

/// The flat keys the arena fixes, each with the reason its refusal
/// names.
const FIXED: &[FixedKey] = &[
    (
        "visibility",
        "the arena is single × team × always-full (team fog of war in one room)",
    ),
    (
        "topology",
        "the arena is single × team × always-full (one room, never sharded)",
    ),
    (
        "communication",
        "the arena is single × team × always-full (its team room sends full snapshots)",
    ),
    ("shard_count", "the arena is one room, never sharded"),
    (
        "aoi_cell_size",
        "the arena runs no AOI cell grid (its fog is the 3D vision preset)",
    ),
    (
        "team_vision_radius",
        "the arena's vision radius is the game's own (gsb_demo_arena::VISION_RADIUS, 3D)",
    ),
    (
        "spawn_half_size",
        "arena units spawn at their team's base on the arena's own 100 m floor",
    ),
    (
        "disconnect_grace_secs",
        "the arena reads its grace from `[arena] disconnect_grace_secs`",
    ),
];

/// The keys the `[arena]` table knows.
const KNOWN: &[&str] = &["teams", "disconnect_grace_secs"];

/// What [`GameModule::configure`] settled.
#[derive(Debug, Clone, Copy)]
struct Settings {
    teams: u8,
    grace: Duration,
}

/// The arena module. Unconfigured until the server calls
/// [`GameModule::configure`].
#[derive(Debug, Default)]
pub struct ArenaModule {
    settings: Option<Settings>,
}

impl ArenaModule {
    /// The `game` config key's value (and the settings table's name).
    pub const NAME: &'static str = "arena";

    /// A fresh, unconfigured module.
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh module behind the trait (the catalog's constructor).
    pub(crate) fn boxed() -> Box<dyn GameModule> {
        Box::new(Self::new())
    }
}

impl GameModule for ArenaModule {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn configure(&mut self, raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        let read = || -> Result<Settings, settings::SettingsError> {
            let own = settings::own_table(raw, Self::NAME, FIXED, KNOWN)?;
            let teams = settings::integer(
                own,
                Self::NAME,
                "teams",
                1..=255,
                "a whole number of teams, 1 to 255",
            )?
            .map_or(DEFAULT_TEAMS, |n| n as u8);
            let grace = settings::seconds(own, Self::NAME, "disconnect_grace_secs")?
                .unwrap_or(gsb_kit::DEFAULT_DISCONNECT_GRACE);
            Ok(Settings { teams, grace })
        };
        self.settings = Some(read().map_err(|e| e.into_server(Self::NAME))?);
        Ok(())
    }

    fn register(&self, table: &mut MessageTable) {
        gsb_demo_arena::register(table);
    }

    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let s = self
            .settings
            .expect("the server configures a module before using it");
        parts.spawn(arena_factory(s.teams, s.grace))
    }

    fn describe(&self) -> String {
        match &self.settings {
            None => "arena (unconfigured)".into(),
            Some(s) => format!(
                "arena: single × team × always-full, teams={} vision={} m (3D), \
                 disconnect grace {} then the retreat bot",
                s.teams,
                VISION_RADIUS,
                secs_label(s.grace)
            ),
        }
    }
}

/// One arena room per room id: a fresh `ArenaGame` for `teams` (its
/// round-robin starts over in every room) with the disconnect `grace`
/// ending in the kit's default AI handover — the arena's bot walks the
/// unit back to its base. Group key: the team.
fn arena_factory(teams: u8, grace: Duration) -> RoomFactory<World, Team, (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(arena_room(ArenaGame::with_teams(teams)).with_disconnect_grace(grace))
            as Box<dyn RoomLogic<World, GroupKey = Team, Strip = ()>>,
    })
}

#[cfg(test)]
mod tests;
