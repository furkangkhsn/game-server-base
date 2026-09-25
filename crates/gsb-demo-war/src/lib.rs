//! "Cephe" — a three-faction war over a sharded map: gsb-kit's FOURTH
//! validation demo (`docs/KIT-ARCHITECTURE.md` §10 "W2 sonucu"), the
//! game the `team × sharded` composite was built for
//! (`docs/CROSS-SHARD.md` §8b).
//!
//! A large 3D map (metres, y up) on a 2×2 shard grid, three factions
//! with their bases in three of the regions, a contested fourth region
//! with two capture points. The visibility model is **team fog of war
//! over the whole sharded map**: a player sees every unit of its faction
//! wherever it stands, and an enemy only while some unit of its faction
//! — a player or one of the faction's watchtowers, one in every region —
//! sees it, on any shard. The kit's [`ShardedTeamRoom`] over the
//! ground-plane presets [`VisionGrid2`] and [`GridPartition2`] (through
//! `Planar` = `[x, z]`), in the team room's delta mode.
//!
//! **An acceptance test of the kit, not a kit user with privileges:**
//! this crate uses only gsb-kit's PUBLIC surface (it cannot reach a
//! `pub(crate)` item — the proof is structural) and depends on no other
//! demo. What it brings is what KIT-ARCHITECTURE §2 calls "the game's":
//!
//! - [`components`] — [`Pos3`] (with `Planar` = `[x, z]`), the unit's
//!   kind / faction / hit points, the walk, a capture point's state;
//! - [`codec`] — its `RecordCodec`: decimetre-quantized 3D position +
//!   kind + faction + hit points;
//! - [`world`] — the map, the shard grid, the vision radius, bases,
//!   towers and capture points;
//! - [`realm`] — saved characters (faction + position) by authenticated
//!   identity, and the faction rule for everyone else;
//! - [`game`] — its `Game`, `TeamGame` and `ShardGame` hooks;
//! - [`combat`] + [`effect`] — attacks on enemies, on this shard's units
//!   and ACROSS a seam through the kit's remote effects;
//! - [`war`] + [`op`] — its wire (`proto/war.proto`) and opcodes.
//!
//! Everything around the hooks — wire identity, the per-faction content
//! built from own + lent + imported records, the export to the registry
//! hub and the import back, the set-delta ledger, migration, the park
//! ledger, the envelopes, the input ack — is the kit's (and the core's).
//! Hosted by `gsb-server` as `game = "war"` and driven by `gsb-loadgen
//! --game war` (`docs/GAME-MODULE.md`); its own tests verify it through
//! a real registry (the team hub) and four real shard actors.

pub mod codec;
pub mod combat;
pub mod components;
pub mod effect;
pub mod game;
mod input;
pub mod migrate;
pub mod op;
pub mod realm;
mod systems;
pub mod world;

pub use components::Pos3;
pub use game::WarGame;
pub use migrate::WarMig;
pub use realm::{Realm, Saved};

use gsb_kit::sharded::{ShardedRoom, ShardedTeamRoom};
use gsb_kit::space::{GridPartition2, VisionGrid2};
use gsb_protocol::MessageTable;

/// Generated war protocol messages (package `gsb.war`, file
/// `war.proto`): its own messages and its typed mirrors of the kit's
/// envelope. The kit's `InputAck` is used as is and re-exported here.
pub mod war {
    include!(concat!(env!("OUT_DIR"), "/gsb.war.rs"));

    pub use gsb_kit::proto::InputAck;
}

/// One shard of the war room: the kit's `team × sharded` composite
/// running [`WarGame`] over the ground-plane shard grid and vision.
pub type WarShard = ShardedTeamRoom<WarGame, GridPartition2<Pos3>, VisionGrid2<Pos3>>;

/// Shard `index` of the war room over `realm` (every shard of the room
/// is built from the same realm), shipping DELTA snapshots — a faction's
/// view is its whole army plus what it sees, and most of it walks: a
/// full every tick re-sent every standing tower too. The export budget
/// and the park grace are the kit's defaults (`with_team_budget`,
/// `with_disconnect_grace` on the result change them).
/// (A free function, not a constructor: the room type is the kit's, so
/// an inherent impl here is E0116.)
#[must_use]
pub fn war_shard(index: usize, realm: &Realm) -> WarShard {
    let shard = ShardedRoom::with_game(WarGame::for_shard(index, realm), world::partition(), index);
    ShardedTeamRoom::with_shard(shard, world::vision(), world::lent_pos).with_delta()
}

/// Register the war game's wire messages with `table` (the server builds
/// one table at startup: base messages + a game's messages).
pub fn register(table: &mut MessageTable) {
    table.reg::<war::MoveTo>(op::WAR_MOVE_TO);
    table.reg::<war::WorldSnapshot>(op::WAR_SNAPSHOT);
    table.reg::<war::Private>(op::WAR_PRIVATE);
    table.reg::<war::Attack>(op::WAR_ATTACK);
}
