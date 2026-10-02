//! [`Target`]: an entity as the MMO's combat reads it — the same fields
//! from one of this shard's entities or from a neighbour's lent record:
//! the kit's `SeamView`, through which `Seam::find` answers a wire id
//! with the seam's precedence (KIT-ARCHITECTURE §10 "F6", BACKLOG F26).

use bevy_ecs::prelude::{Entity, World};
use bevy_ecs::world::EntityRef;
use gsb_kit::identity::WireId;
use gsb_kit::sharded::{Found, Holder, SeamView};

use crate::codec::{MmoWire, from_dm};
use crate::components::{Pos3, Vitals};

/// An entity as an attack reads it — one of this shard's or a
/// neighbour's lent record, the same fields either way.
#[derive(Debug, Clone, Copy)]
pub(super) struct Target {
    pub(super) at: Pos3,
    /// It has hit points left (a mob alive, a player not defeated).
    pub(super) up: bool,
}

impl SeamView<MmoWire> for Target {
    fn local(entity: EntityRef<'_>) -> Option<Self> {
        let at = *entity.get::<Pos3>()?;
        let up = entity.get::<Vitals>().is_some_and(|v| v.hp > 0);
        Some(Target { at, up })
    }

    fn lent(w: &MmoWire) -> Option<Self> {
        let at = Pos3::new(from_dm(w.x), from_dm(w.y), from_dm(w.z));
        Some(Target { at, up: w.hp > 0 })
    }
}

/// With no seam (a single-world room): the entity with wire id `wire`
/// in this world.
pub(super) fn local_target(world: &mut World, wire: u64) -> Option<Found<Target>> {
    let mut q = world.query::<(Entity, &WireId)>();
    let (entity, _) = q.iter(world).find(|(_, w)| w.get() == wire)?;
    let view = Target::local(world.entity(entity))?;
    let holder = Holder::Local(entity);
    Some(Found { wire, holder, view })
}
