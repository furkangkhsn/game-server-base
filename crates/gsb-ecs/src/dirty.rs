//! Explicit, version-based dirty tracking.
//!
//! Every entity the game wants to broadcast carries an [`EntityVersion`]
//! component. Any mutation (ingest or system) calls [`bump`]. The broadcast
//! phase compares the current version against the last sent one and only
//! encodes entities that actually changed. This is deterministic and
//! independent of ECS change-detection internals.

use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::Component;
use bevy_ecs::world::World;

/// Monotonic version counter; bumped on every meaningful mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Component, Default)]
pub struct EntityVersion(pub u64);

/// Increment the version of `entity` (inserts the component if missing).
/// No-op if the entity no longer exists.
pub fn bump(world: &mut World, entity: Entity) {
    if world.get_entity(entity).is_err() {
        return;
    }
    if let Some(mut version) = world.entity_mut(entity).get_mut::<EntityVersion>() {
        version.0 += 1;
    } else {
        world.entity_mut(entity).insert(EntityVersion(1));
    }
}
