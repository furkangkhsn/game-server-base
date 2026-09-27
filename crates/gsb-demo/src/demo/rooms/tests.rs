//! The kit's rooms running the DEMO game, pinned through what the demo's
//! codec writes: every assertion here reads a record VALUE (a decoded,
//! truncated coordinate) or depends on the demo's quantization, so these
//! tests belong to the game whose bytes they pin, not to the kit (whose
//! own tests run a fixture game).
//!
//! They were the kit's in-module tests until the crate split
//! (KIT-ARCHITECTURE §10, phase 2); each kept its name and its
//! assertions. Only the plumbing changed: a player's entity is found
//! through its public wire id instead of the room's private player
//! table.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::room::{GameLogic, TickCtx};
use prost::Message;

use crate::demo::components::{DEFAULT_SPEED, Position, Speed};
use crate::demo::game::WorldSnapshot;
use crate::prelude::*;
use gsb_kit::identity::WireId;

mod aoi;
mod economy;
mod open;
mod pvs;
mod sharded;

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: Default::default(),
    }
}

fn ctx1() -> TickCtx<'static> {
    ctx(1)
}

/// The entity carrying wire id `wire`.
fn entity_of(world: &mut World, wire: u64) -> Entity {
    world
        .query::<(Entity, &WireId)>()
        .iter(world)
        .find_map(|(e, w)| (w.get() == wire).then_some(e))
        .expect("an entity with this wire id")
}

/// Join a player on `room` and move its entity to an exact position.
/// Returns the wire id.
fn place<R: GameLogic<World>>(
    world: &mut World,
    room: &mut R,
    conn: ConnectionId,
    x: f32,
    y: f32,
) -> u64 {
    let admission = room.on_join(world, conn);
    let entity = entity_of(world, admission.entity);
    world.entity_mut(entity).insert(Position { x, y });
    admission.entity
}

fn decode(out: &bytes::BytesMut) -> WorldSnapshot {
    WorldSnapshot::decode(out.as_ref()).expect("snapshot payload")
}

fn ids(snap: &WorldSnapshot) -> BTreeSet<u64> {
    snap.entities.iter().map(|e| e.entity).collect()
}

fn snap_ids(out: &bytes::BytesMut) -> BTreeSet<u64> {
    ids(&decode(out))
}
