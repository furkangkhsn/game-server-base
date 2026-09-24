//! Where (and as what) the demo's players enter the world: the spawn
//! map size, the deterministic spawn distribution, the player's
//! component bundle, the join-time team assignment, and the rebuild of
//! an entity that migrated in from a neighbouring shard.

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::ConnectionId;

use crate::demo::components::{DEFAULT_SPEED, MoveTarget, Position, Speed};
use crate::kit::identity::WireId;
use crate::kit::team::{TEAM_COUNT, Team};

/// The default spawn map half-size (world units): the historical 100×100
/// arena. A room built with it spawns bit-identically to the pre-config
/// `spawn_pos`.
pub const DEFAULT_SPAWN_HALF: f32 = 50.0;

/// Deterministic pseudo-random spawn point in a square arena of half-size
/// `half`, derived from the connection id (stable across room re-joins in
/// the same session). `half = 50` reproduces the historical 100×100 arena
/// exactly: the same 1000×1000 lattice, just scaled. `pub` so the other
/// rooms share the exact same spawn distribution (a fair comparison in
/// the load generator) and the sharded room factory can route a join to
/// the home shard by computing the spawn position's region.
pub fn spawn_pos(conn: ConnectionId, half: f32) -> (f32, f32) {
    // The historical 100×100 lattice, scaled: `half = 50` multiplies by
    // exactly 1.0, so the default is bit-identical to the pre-config
    // formula (a re-derivation like `(h % 1000) * 2 * half / 1000` would
    // double-round and drift by ulps for some ids).
    let h = conn.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let scale = half / 50.0;
    let x = ((h % 1000) as f32 / 10.0 - 50.0) * scale;
    let y = (((h >> 32) % 1000) as f32 / 10.0 - 50.0) * scale;
    (x, y)
}

/// A joining player's component bundle: the deterministic spawn point on
/// the room's spawn map (derived from the TRANSPORT session id, as it
/// always was — the load generator's home distribution pairs with it)
/// and the default speed. `DemoGame::spawn_player` spawns exactly this.
pub(crate) fn player_bundle(conn: ConnectionId, spawn_half: f32) -> (Position, Speed) {
    let (x, y) = spawn_pos(conn, spawn_half);
    (Position { x, y }, Speed(DEFAULT_SPEED))
}

/// Spawn a joining player's entity with the wire identity the kit minted
/// for it — the spawn path of the rooms not yet generic over the game
/// (team, PVS, sharded — phase 1b); the generic rooms call
/// `DemoGame::spawn_player` and stamp the identity themselves.
pub(crate) fn spawn_player(
    world: &mut World,
    conn: ConnectionId,
    spawn_half: f32,
    wire: WireId,
) -> Entity {
    world.spawn((player_bundle(conn, spawn_half), wire)).id()
}

/// The join-time team *assignment rule* (the demo: conn parity, i.e.
/// "signup order" — team 0, 1, 0, 1, …). This decides what the team
/// room's `on_join` *writes* into the entity's
/// [`TeamMember`](crate::kit::team::TeamMember); it is not
/// consulted again afterwards (runtime team changes are component
/// writes, and `group_of` reads the world, not this function). The
/// future `Game::on_player_spawned` (KIT-ARCHITECTURE §4.3).
#[inline]
pub(crate) fn team_of(conn: ConnectionId) -> Team {
    Team((conn.0 % u64::from(TEAM_COUNT)) as u8)
}

/// Rebuild a migrated entity on the receiving shard from the game state
/// it carried (position, speed, pending move target), keeping the wire
/// identity it travelled with — the future `Game::restore`
/// (KIT-ARCHITECTURE §4.3). The kit keeps the bookkeeping around it
/// (wire/player tables, the park record).
pub(crate) fn restore_migrant(
    world: &mut World,
    wire: WireId,
    pos: Position,
    speed: f32,
    target: Option<MoveTarget>,
) -> Entity {
    let entity = world.spawn((pos, Speed(speed), wire)).id();
    if let Some(target) = target {
        world.entity_mut(entity).insert(target);
    }
    entity
}
