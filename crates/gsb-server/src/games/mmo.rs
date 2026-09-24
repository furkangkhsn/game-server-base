//! The 3D MMO (`gsb-demo-mmo`) as a game module: ONE sharded world per
//! room — the MMO's 2×2 shard grid, every shard the kit's `sharded ×
//! spatial` composite with the ground-plane grid AOI
//! (`ShardedSpatialRoom<MmoGame, GridPartition2<Pos3>, Grid2>`).
//!
//! **Join routing** (GAME-MODULE §6 decision 6): a session with a saved
//! character goes to the shard owning its saved position
//! (`world::home_shard`); a session with none goes to the shard of the
//! DEFAULT waystone ([`DEFAULT_WAYSTONE`]) — where that shard's
//! `spawn_player` puts a character with no save, so the router and the
//! spawn agree. The core hands the router the transport session only
//! (no account), so the saved characters are keyed by session, exactly
//! as `Realm::logins` is; a server with no login step (this one) has no
//! saves and routes everyone to the default waystone.
//!
//! **Disconnects** (§6 decision 5): the MMO's own logout timer — the
//! character stays parked for the logout grace, then logs out
//! (`ExpireTo::Despawn`, the slot is released) unless it is in combat
//! (`MmoGame::may_release` holds it until the fight cools down, bounded
//! by the core's `max_detach_hold`). The `[mmo]` table sets it:
//!
//! - `logout_grace_secs` — default the MMO's [`LOGOUT_GRACE`] (20 s);
//!   `0` = log out at once (no park, so no combat hold either);
//! - `logout` — `"release"` (default: the slot is released) or `"bot"`
//!   (the MMO's logout bot walks the character to the nearest waystone
//!   and it stays, slot held).
//!
//! The demo's flat `disconnect_grace_secs` written explicitly REFUSES
//! startup instead of being mapped: its meaning (a MOBA-style hold that
//! ends in a bot) is not the MMO's logout, and a silently reinterpreted
//! key would mislead an operator switching games. Refused too: the three
//! axes (sharded × spatial × delta is the MMO), `shard_count` and
//! `aoi_cell_size` (fixed by the world and by the client's cell formula),
//! `team_vision_radius` and `spawn_half_size`.

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::world::World;
use gsb_core::registry::{BuiltRoom, RoomFactory, Shard};
use gsb_core::room::ExpireTo;
use gsb_core::shard::ShardLogic;
use gsb_demo_mmo::codec::MmoWire;
use gsb_demo_mmo::world::{SHARDS, WAYSTONES, home_shard};
use gsb_demo_mmo::{LOGOUT_GRACE, MmoMig, Pos3, Realm, mmo_shard};
use gsb_kit::sharded::KitMig;
use gsb_kit::space::Cell;
use gsb_protocol::MessageTable;

use super::settings::{self, FixedKey, secs_label};
use crate::{Config, GameModule, RegistryParts, RegistryTask, ServerError};

/// The waystone a session with no saved character is routed to (and
/// spawned at: the routed shard's own waystone — see [`default_shard`]).
pub const DEFAULT_WAYSTONE: usize = 0;

/// The flat keys the MMO fixes, each with the reason its refusal names.
const FIXED: &[FixedKey] = &[
    (
        "visibility",
        "the MMO is sharded × spatial × delta (a ground-plane grid AOI over its shard grid)",
    ),
    (
        "topology",
        "the MMO is sharded × spatial × delta (its world is always a shard grid)",
    ),
    (
        "communication",
        "the MMO is sharded × spatial × delta (its cell-delta snapshots)",
    ),
    (
        "shard_count",
        "the MMO's world is a 2×2 shard grid (gsb_demo_mmo::world::SHARDS); its waystones, \
         camps and join routing are laid out on it",
    ),
    (
        "aoi_cell_size",
        "the MMO's AOI cell is 64 m (gsb_demo_mmo::world::CELL_SIZE), the edge its clients \
         divide record coordinates by",
    ),
    ("team_vision_radius", "the MMO has no team fog of war"),
    (
        "spawn_half_size",
        "MMO characters spawn at their saved position or a waystone of the MMO's own map",
    ),
    (
        "disconnect_grace_secs",
        "the MMO's disconnect is its logout timer: set `[mmo] logout_grace_secs` \
         (and `[mmo] logout`)",
    ),
];

/// The keys the `[mmo]` table knows.
const KNOWN: &[&str] = &["logout_grace_secs", "logout"];

/// What [`GameModule::configure`] settled: the logout policy.
#[derive(Debug, Clone, Copy)]
struct Settings {
    grace: Duration,
    to: ExpireTo,
}

/// The MMO module over a realm (the saved characters and the mob spawn
/// table every room's shards are built from). Unconfigured until the
/// server calls [`GameModule::configure`].
#[derive(Debug)]
pub struct MmoModule {
    realm: Arc<Realm>,
    settings: Option<Settings>,
}

impl Default for MmoModule {
    fn default() -> Self {
        Self::new()
    }
}

impl MmoModule {
    /// The `game` config key's value (and the settings table's name).
    pub const NAME: &'static str = "mmo";

    /// A fresh module over the live realm ([`Realm::standard`]: the camps,
    /// the wolf packs and the flyer; no saved characters).
    pub fn new() -> Self {
        Self::with_realm(Realm::standard())
    }

    /// A fresh module over `realm` — an embedder's content and character
    /// data (hosted through `start_game_server`).
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

/// The shard a session with no saved character is routed to: the one
/// owning [`DEFAULT_WAYSTONE`]. Its `spawn_player` falls back to ITS
/// waystone, which is that same waystone (waystone `i` lies in shard
/// `i`'s region — pinned by a unit test).
pub fn default_shard() -> usize {
    let [x, z] = WAYSTONES[DEFAULT_WAYSTONE];
    home_shard(&Pos3::new(x, 0.0, z))
}

/// The MMO's join router over `realm`'s saved characters.
pub fn route(realm: &Realm, conn: gsb_core::id::ConnectionId) -> usize {
    realm
        .logins
        .get(&conn)
        .map_or_else(default_shard, home_shard)
}

impl GameModule for MmoModule {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn configure(&mut self, raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        let read = || -> Result<Settings, settings::SettingsError> {
            let own = settings::own_table(raw, Self::NAME, FIXED, KNOWN)?;
            let grace =
                settings::seconds(own, Self::NAME, "logout_grace_secs")?.unwrap_or(LOGOUT_GRACE);
            let to = match settings::choice(
                own,
                Self::NAME,
                "logout",
                &["release", "bot"],
                "\"release\" (the slot is released) or \"bot\" (the logout bot)",
            )? {
                Some("bot") => ExpireTo::AiHandover,
                _ => ExpireTo::Despawn,
            };
            Ok(Settings { grace, to })
        };
        self.settings = Some(read().map_err(|e| e.into_server(Self::NAME))?);
        Ok(())
    }

    fn register(&self, table: &mut MessageTable) {
        gsb_demo_mmo::register(table);
    }

    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let s = self
            .settings
            .expect("the server configures a module before using it");
        parts.spawn(mmo_factory(Arc::clone(&self.realm), s.grace, s.to))
    }

    fn describe(&self) -> String {
        match &self.settings {
            None => "mmo (unconfigured)".into(),
            Some(s) => format!(
                "mmo: sharded × spatial × delta, {SHARDS} shards, logout after {} ({}), \
                 held while in combat; unsaved sessions → waystone {DEFAULT_WAYSTONE} (shard {})",
                secs_label(s.grace),
                match s.to {
                    ExpireTo::Despawn => "slot released",
                    ExpireTo::AiHandover => "logout bot",
                },
                default_shard(),
            ),
        }
    }
}

/// The trait object one MMO shard runs as.
type MmoShardLogic =
    dyn ShardLogic<World, GroupKey = Cell, State = KitMig<MmoMig>, Strip = MmoWire>;

/// One whole sharded MMO world per room id: [`SHARDS`] shards built from
/// the same realm, each with the logout policy, and the join router.
fn mmo_factory(
    realm: Arc<Realm>,
    grace: Duration,
    to: ExpireTo,
) -> RoomFactory<World, Cell, KitMig<MmoMig>, MmoWire> {
    Arc::new(move |_id, _config| {
        let shards: Vec<Shard<World, Cell, KitMig<MmoMig>, MmoWire>> = (0..SHARDS)
            .map(|i| {
                let shard = mmo_shard(i, &realm).with_disconnect_policy(Some(grace), to);
                (World::new(), Box::new(shard) as Box<MmoShardLogic>)
            })
            .collect();
        let realm = Arc::clone(&realm);
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |conn| route(&realm, conn)),
        }
    })
}

#[cfg(test)]
mod tests;
