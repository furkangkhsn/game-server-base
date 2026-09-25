//! "Cephe" (`gsb-demo-war`) as a game module: ONE sharded world per room
//! — the war's 2×2 shard grid, every shard the kit's `team × sharded`
//! composite with team fog over the whole map, in the team room's delta
//! mode (`ShardedTeamRoom<WarGame, GridPartition2<Pos3>, VisionGrid2<Pos3>>`,
//! built by `gsb_demo_war::war_shard`). The shards' faction views travel
//! through the registry's team hub (`docs/CROSS-SHARD.md` §8b).
//!
//! **Join routing** (K4): a player goes to the shard owning its
//! placement — its saved character (faction + position), or, unsaved,
//! its faction's base, the faction dealt by the war's deterministic rule
//! (`gsb_demo_war::realm::faction_of`, a hash of the identity) — by the
//! identity the core hands the router: the ticket's validated player, or
//! the client-claimed `Auth.name` on the local-auth development path.
//! The spawn reads the same realm by the same identity, so the two
//! agree. The catalog's realm has no saved characters: an embedder
//! brings its character data with [`WarModule::with_realm`].
//!
//! Settings come from the `[war]` table (GAME-MODULE §4.3):
//!
//! - `disconnect_grace_secs` — how long a dropped player's unit is held
//!   (parked, resumable) before the war's retreat bot walks it home to
//!   its base (default the kit's 30 s; `0` = the unit leaves at once);
//! - `team_budget` — the most records a shard exports per faction per
//!   tick (members first, then the enemies they see; default the kit's
//!   `DEFAULT_TEAM_BUDGET`, 1 024; at most the core's per-message cap).
//!
//! Everything else the war fixes, and an explicitly written flat key
//! for it refuses startup: the three axes, `shard_count`,
//! `aoi_cell_size`, `team_vision_radius` (the war's own 60 m, on the
//! ground), `spawn_half_size` (units spawn at their saved spot or their
//! base), and the demo's flat `disconnect_grace_secs` (the war reads its
//! own, above).

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::world::World;
use gsb_core::registry::{BuiltRoom, RoomFactory, Shard};
use gsb_core::shard::{ShardLogic, TEAM_EXPORT_MAX_RECORDS};
use gsb_demo_war::codec::WarWire;
use gsb_demo_war::world::{FACTIONS, SHARDS, VISION_RADIUS, home_shard};
use gsb_demo_war::{Realm, WarMig, war_shard};
use gsb_kit::sharded::{DEFAULT_TEAM_BUDGET, TeamMig};
use gsb_kit::team::Team;
use gsb_protocol::MessageTable;

use super::settings::{self, FixedKey, secs_label};
use crate::{Config, GameModule, RegistryParts, RegistryTask, ServerError};

/// The flat keys the war fixes, each with the reason its refusal names.
const FIXED: &[FixedKey] = &[
    (
        "visibility",
        "the war is sharded × team × delta (team fog of war over its shard grid)",
    ),
    (
        "topology",
        "the war is sharded × team × delta (its map is always a shard grid)",
    ),
    (
        "communication",
        "the war is sharded × team × delta (its faction views ship as deltas)",
    ),
    (
        "shard_count",
        "the war's map is a 2×2 shard grid (gsb_demo_war::world::SHARDS); its bases, \
         towers and capture points are laid out on it",
    ),
    (
        "aoi_cell_size",
        "the war runs no AOI cell grid (its fog is the ground-plane vision preset)",
    ),
    (
        "team_vision_radius",
        "the war's vision radius is the game's own (gsb_demo_war::world::VISION_RADIUS, \
         on the ground)",
    ),
    (
        "spawn_half_size",
        "war units spawn at their saved position or their faction's base on the war's own map",
    ),
    (
        "disconnect_grace_secs",
        "the war reads its grace from `[war] disconnect_grace_secs`",
    ),
];

/// The keys the `[war]` table knows.
const KNOWN: &[&str] = &["disconnect_grace_secs", "team_budget"];

/// What [`GameModule::configure`] settled.
#[derive(Debug, Clone, Copy)]
struct Settings {
    grace: Duration,
    budget: usize,
}

/// The war module over a realm (the saved characters every room's
/// shards and router read). Unconfigured until the server calls
/// [`GameModule::configure`].
#[derive(Debug)]
pub struct WarModule {
    realm: Arc<Realm>,
    settings: Option<Settings>,
}

impl Default for WarModule {
    fn default() -> Self {
        Self::new()
    }
}

impl WarModule {
    /// The `game` config key's value (and the settings table's name).
    pub const NAME: &'static str = "war";

    /// A fresh module over an empty realm: every player is placed by the
    /// faction rule, at its faction's base.
    pub fn new() -> Self {
        Self::with_realm(Realm::empty())
    }

    /// A fresh module over `realm` — an embedder's character data
    /// (hosted through `start_game_server`).
    pub fn with_realm(realm: Realm) -> Self {
        Self {
            realm: Arc::new(realm),
            settings: None,
        }
    }

    /// A fresh module behind the trait (the catalog's constructor).
    pub(crate) fn boxed() -> Box<dyn GameModule> {
        Box::new(Self::new())
    }
}

/// The war's join router over `realm`: the shard owning the identity's
/// placement.
pub fn route(realm: &Realm, identity: &str) -> usize {
    home_shard(&realm.placement(identity).at)
}

impl GameModule for WarModule {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn configure(&mut self, raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        let read = || -> Result<Settings, settings::SettingsError> {
            let own = settings::own_table(raw, Self::NAME, FIXED, KNOWN)?;
            let grace = settings::seconds(own, Self::NAME, "disconnect_grace_secs")?
                .unwrap_or(gsb_kit::DEFAULT_DISCONNECT_GRACE);
            let budget = settings::integer(
                own,
                Self::NAME,
                "team_budget",
                1..=TEAM_EXPORT_MAX_RECORDS as i64,
                "a whole number of records, 1 to 16384 (the core's per-export cap)",
            )?
            .map_or(DEFAULT_TEAM_BUDGET, |n| n as usize);
            Ok(Settings { grace, budget })
        };
        self.settings = Some(read().map_err(|e| e.into_server(Self::NAME))?);
        Ok(())
    }

    fn register(&self, table: &mut MessageTable) {
        gsb_demo_war::register(table);
    }

    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let s = self
            .settings
            .expect("the server configures a module before using it");
        parts.spawn(war_factory(Arc::clone(&self.realm), s))
    }

    fn describe(&self) -> String {
        match &self.settings {
            None => "war (unconfigured)".into(),
            Some(s) => format!(
                "war: sharded × team × delta, {SHARDS} shards, {FACTIONS} factions, vision \
                 {VISION_RADIUS} m (ground), export budget {} records/faction/tick, \
                 disconnect grace {} then the retreat bot",
                s.budget,
                secs_label(s.grace)
            ),
        }
    }
}

/// The trait object one war shard runs as.
type WarShardLogic =
    dyn ShardLogic<World, GroupKey = Team, State = TeamMig<WarMig>, Strip = WarWire>;

/// One whole sharded war per room id: [`SHARDS`] shards built from the
/// same realm, each with the grace and the export budget, and the join
/// router.
fn war_factory(
    realm: Arc<Realm>,
    s: Settings,
) -> RoomFactory<World, Team, TeamMig<WarMig>, WarWire> {
    Arc::new(move |_id, _config| {
        let shards: Vec<Shard<World, Team, TeamMig<WarMig>, WarWire>> = (0..SHARDS)
            .map(|i| {
                let shard = war_shard(i, &realm)
                    .with_team_budget(s.budget)
                    .with_disconnect_grace(s.grace);
                (World::new(), Box::new(shard) as Box<WarShardLogic>)
            })
            .collect();
        let realm = Arc::clone(&realm);
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |_conn, identity: &str| route(&realm, identity)),
        }
    })
}

#[cfg(test)]
mod tests;
