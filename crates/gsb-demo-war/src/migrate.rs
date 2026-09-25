//! What travels with a unit across a shard border — the game's
//! `ShardGame::Mig` ([`WarMig`]) and its `capture` / `restore`.
//!
//! The KIT carries the rest: the wire id, the owning player, a parked
//! player's park record — and the unit's team (the kit's `TeamMember`
//! rides in the composite's `TeamMig`, written back on arrival). Only
//! players ever cross: towers and capture points stand still in their
//! own regions. The state is still whole for any unit, so nothing is
//! lost should one ever move.

use bevy_ecs::prelude::{Entity, World};

use crate::components::{Capture, MoveTarget, Pos3, Unit};

/// A migrating unit's game state.
#[derive(Debug, Clone, PartialEq)]
pub struct WarMig {
    pub pos: Pos3,
    pub unit: Unit,
    /// The pending walk: a player running across a seam keeps running.
    pub target: Option<MoveTarget>,
    /// A capture point's state.
    pub capture: Option<Capture>,
}

/// Capture `entity`'s state on the sending shard.
pub(crate) fn capture(world: &World, entity: Entity) -> WarMig {
    let e = world.entity(entity);
    WarMig {
        pos: e.get::<Pos3>().copied().unwrap_or_default(),
        unit: *e.get::<Unit>().expect("every broadcast unit has a Unit"),
        target: e.get::<MoveTarget>().copied(),
        capture: e.get::<Capture>().copied(),
    }
}

/// Rebuild a migrated unit on the receiving shard (the kit stamps the
/// wire id and the team it travelled with right after).
pub(crate) fn restore(world: &mut World, mig: WarMig) -> Entity {
    let mut e = world.spawn((mig.pos, mig.unit));
    if let Some(target) = mig.target {
        e.insert(target);
    }
    if let Some(capture) = mig.capture {
        e.insert(capture);
    }
    e.id()
}
