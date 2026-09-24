//! The 3D MMO — gsb-kit's third validation demo and the closing check
//! (`docs/KIT-ARCHITECTURE.md` §11–§13, phase 4).
//!
//! A small MMO world whose visibility model is **a spatial grid AOI over
//! a sharded world**: the kit's `sharded × spatial` composite
//! ([`ShardedSpatialRoom`]) with the 2D presets [`Grid2`] (AOI cells) and
//! [`GridPartition2`] (the shard grid) working on the **ground plane** of
//! 3D data through the kit's `Planar` accessor (`[x, z]`). Positions are
//! 3D (metres, y up) and flyers live high above the ground, but interest
//! management deliberately ignores height — the usual MMO choice, and the
//! opposite of the arena's volumetric fog.
//!
//! **An acceptance test of the kit, not a kit user with privileges:**
//! this crate uses only gsb-kit's PUBLIC surface (it cannot reach a
//! `pub(crate)` item — the proof is structural) and depends on neither
//! other demo. What it brings is exactly what KIT-ARCHITECTURE §2 calls
//! "the game's":
//!
//! - [`components`] — [`Pos3`] (with `Planar` = `[x, z]`), vitals, the
//!   players' walk, the mobs' brain;
//! - [`codec`] — its `RecordCodec`: decimetre-quantized 3D position +
//!   kind + hit points, `Dirty` = position or vitals changed;
//! - [`world`] — the map, the shard grid and the AOI grid parameters;
//! - [`realm`] — saved characters and the mob spawn table;
//! - [`game`] — its `Game` and `ShardGame` hooks (spawn, input, camps,
//!   mob routes and lifetimes, player movement, the optional logout bot,
//!   the migrating state [`MmoMig`]);
//! - [`mmo`] + [`op`] — its wire (`proto/mmo.proto`) and opcodes.
//!
//! Everything around the hooks — wire identity, AOI grouping, the
//! cell-delta engine with its border-strip ledger, migration routing,
//! the park ledger, the snapshot and `Private` envelopes, the input ack —
//! is the kit's. Not wired into `gsb-server` or the load generator (§12,
//! out of scope): the MMO is verified by its own tests, through the real
//! `gsb-core` shard actors.

pub mod codec;
pub mod components;
pub mod game;
mod input;
pub mod migrate;
pub mod op;
pub mod realm;
mod systems;
pub mod world;

pub use components::Pos3;
pub use game::MmoGame;
pub use migrate::MmoMig;
pub use realm::{MobSpawn, Realm};

use std::time::Duration;

use gsb_core::room::ExpireTo;
use gsb_kit::sharded::{ShardedRoom, ShardedSpatialRoom};
use gsb_kit::space::{Grid2, GridPartition2};
use gsb_protocol::MessageTable;

/// Generated MMO protocol messages (package `gsb.mmo`, file `mmo.proto`):
/// its own messages and its typed mirrors of the kit's envelope. The
/// kit's `InputAck` is used as is and re-exported here.
pub mod mmo {
    include!(concat!(env!("OUT_DIR"), "/gsb.mmo.rs"));

    pub use gsb_kit::proto::InputAck;
}

/// One shard of the MMO room: the kit's `sharded × spatial` composite
/// running [`MmoGame`] over the ground-plane shard grid and AOI grid.
pub type MmoShard = ShardedSpatialRoom<MmoGame, GridPartition2<Pos3>, Grid2>;

/// The logout timer: how long a disconnected character stays in the
/// world (parked, slot held, resumable) before the logout releases its
/// slot (the room's disconnect grace) — or longer, while it is in combat
/// ([`game::MmoGame`]'s `may_release` veto).
pub const LOGOUT_GRACE: Duration = Duration::from_secs(20);

/// Shard `index` of the MMO room over `realm` (every shard of the room is
/// built from the same realm), with the MMO's logout timer: a hold of
/// [`LOGOUT_GRACE`] that ends by RELEASING the slot
/// ([`ExpireTo::Despawn`]), and not while the character is in combat (the
/// game's veto; the room config's `max_detach_hold` bounds how long a
/// fight can hold it). An operator who prefers the logout bot (the
/// character walks to the nearest waystone and stays, slot held) rebuilds
/// it with `.with_disconnect_policy(Some(grace), ExpireTo::AiHandover)`.
/// (A free function, not a constructor: the room type is the kit's, so
/// an inherent impl here is E0116.)
#[must_use]
pub fn mmo_shard(index: usize, realm: &Realm) -> MmoShard {
    let shard = ShardedRoom::with_game(MmoGame::for_shard(index, realm), world::partition(), index);
    ShardedSpatialRoom::with_shard(shard, world::aoi_grid())
        .with_disconnect_policy(Some(LOGOUT_GRACE), ExpireTo::Despawn)
}

/// Register the MMO's wire messages with `table` (the server builds one
/// table at startup: base messages + a game's messages).
pub fn register(table: &mut MessageTable) {
    table.reg::<mmo::MoveTo>(op::MMO_MOVE_TO);
    table.reg::<mmo::WorldSnapshot>(op::MMO_SNAPSHOT);
    table.reg::<mmo::Private>(op::MMO_PRIVATE);
    table.reg::<mmo::Attack>(op::MMO_ATTACK);
    table.reg::<mmo::Travel>(op::MMO_TRAVEL);
}
