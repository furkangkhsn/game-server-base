//! What travels with an entity across a shard border — the MMO's
//! `ShardGame::Mig` ([`MmoMig`]) and its `capture` / `restore`.
//!
//! The KIT carries the rest (KIT-ARCHITECTURE §4.6): the wire id (the
//! client sees the same entity), the owning player, and a parked
//! player's park record. Which entities migrate is the kit's rule too:
//! every entity with the codec's marker whose ground position lies in
//! another region — mobs included, although they carry no speed-like
//! component (§8.5).

use bevy_ecs::prelude::{Entity, World};

use crate::components::{Mob, MoveTarget, Pos3, RunSpeed, Vitals};

/// A migrating entity's game state: a player's or a mob's.
#[derive(Debug, Clone, PartialEq)]
pub enum MmoMig {
    /// A player: position, vitals, run speed and the pending walk (a
    /// player running across a seam keeps running on the other side).
    Player {
        pos: Pos3,
        vitals: Vitals,
        speed: f32,
        target: Option<MoveTarget>,
    },
    /// A mob: position (a flyer's altitude included), vitals (damage
    /// taken so far) and its whole brain — route, current leg, pace and
    /// the tick it despawns at (it dies on schedule on the new shard).
    Mob { pos: Pos3, vitals: Vitals, mob: Mob },
}

/// Capture `entity`'s state on the sending shard. An entity with a
/// [`Mob`] brain is a mob; anything else broadcast is a player.
pub(crate) fn capture(world: &World, entity: Entity) -> MmoMig {
    let e = world.entity(entity);
    let pos = e.get::<Pos3>().copied().unwrap_or_default();
    let vitals = e
        .get::<Vitals>()
        .copied()
        .expect("every broadcast entity has vitals");
    match e.get::<Mob>() {
        Some(mob) => MmoMig::Mob {
            pos,
            vitals,
            mob: mob.clone(),
        },
        None => MmoMig::Player {
            pos,
            vitals,
            speed: e.get::<RunSpeed>().map_or(crate::world::RUN_SPEED, |s| s.0),
            target: e.get::<MoveTarget>().copied(),
        },
    }
}

/// Rebuild a migrated entity on the receiving shard (the kit stamps the
/// wire id it travelled with right after).
pub(crate) fn restore(world: &mut World, mig: MmoMig) -> Entity {
    match mig {
        MmoMig::Player {
            pos,
            vitals,
            speed,
            target,
        } => {
            let mut e = world.spawn((pos, vitals, RunSpeed(speed)));
            if let Some(target) = target {
                e.insert(target);
            }
            e.id()
        }
        MmoMig::Mob { pos, vitals, mob } => world.spawn((pos, vitals, mob)).id(),
    }
}
