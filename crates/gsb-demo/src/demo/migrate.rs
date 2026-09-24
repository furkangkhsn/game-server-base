//! What a demo entity carries across a shard border (the demo's
//! `ShardGame`, KIT-ARCHITECTURE §4.3): its position, its speed if it has
//! one, and its pending move target.

use bevy_ecs::prelude::{Entity, World};

use crate::demo::components::{MoveTarget, Position, Speed};
use crate::demo::play::DemoGame;
use gsb_kit::game::ShardGame;

/// The demo's migrating game state (`ShardGame::Mig`): everything a
/// demo entity carries in its components. The kit wraps it with its own
/// half (the park record) in `KitMig`.
#[derive(Debug, Clone, PartialEq)]
pub struct DemoMig {
    pub pos: Position,
    /// The entity's speed; `None` for an entity without one (an NPC the
    /// game spawned with a position only — it migrates too, §8.5).
    pub speed: Option<f32>,
    pub target: Option<MoveTarget>,
}

impl ShardGame for DemoGame {
    type Mig = DemoMig;

    fn capture(&self, world: &World, entity: Entity) -> DemoMig {
        let e = world.entity(entity);
        DemoMig {
            pos: e.get::<Position>().copied().unwrap_or_default(),
            speed: e.get::<Speed>().map(|s| s.0),
            target: e.get::<MoveTarget>().copied(),
        }
    }

    /// Rebuild the entity from the state it carried, with exactly the
    /// components it had (no `Speed` for an entity that had none).
    fn restore(&mut self, world: &mut World, mig: DemoMig) -> Entity {
        let entity = match mig.speed {
            Some(speed) => world.spawn((mig.pos, Speed(speed))).id(),
            None => world.spawn(mig.pos).id(),
        };
        if let Some(target) = mig.target {
            world.entity_mut(entity).insert(target);
        }
        entity
    }
}
