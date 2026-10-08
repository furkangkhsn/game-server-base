//! What a demo entity carries across a shard border (the demo's
//! `ShardGame`, KIT-ARCHITECTURE §4.3): its position, its speed if it has
//! one, its pending move target, its identity and what it was granted.

use bevy_ecs::prelude::{Entity, World};

use crate::demo::components::{Account, Granted, MoveTarget, Position, Speed};
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
    /// The player's identity, if it has one (round e5).
    pub account: Option<Account>,
    /// What it was granted, if anything (round e5).
    pub granted: Option<Granted>,
}

impl ShardGame for DemoGame {
    type Mig = DemoMig;

    fn capture(&self, world: &World, entity: Entity) -> DemoMig {
        let e = world.entity(entity);
        DemoMig {
            pos: e.get::<Position>().copied().unwrap_or_default(),
            speed: e.get::<Speed>().map(|s| s.0),
            target: e.get::<MoveTarget>().copied(),
            account: e.get::<Account>().cloned(),
            granted: e.get::<Granted>().copied(),
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
        if let Some(account) = mig.account {
            world.entity_mut(entity).insert(account);
        }
        if let Some(granted) = mig.granted {
            world.entity_mut(entity).insert(granted);
        }
        entity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity and the grants cross a shard border with the entity
    /// (round e5): the next grant finds its buyer on the new shard.
    #[test]
    fn the_account_and_its_grants_migrate() {
        let mut game = DemoGame::default();
        let mut from = World::new();
        let e = from
            .spawn((
                Position::default(),
                Account(gsb_core::calls::Identity::new("alice")),
                Granted {
                    items: 2,
                    version: 9,
                },
            ))
            .id();
        let mig = game.capture(&from, e);
        let mut to = World::new();
        let moved = game.restore(&mut to, mig);
        let r = to.entity(moved);
        assert_eq!(r.get::<Account>().map(|a| a.0.as_str()), Some("alice"));
        assert_eq!(
            r.get::<Granted>(),
            Some(&Granted {
                items: 2,
                version: 9
            })
        );
    }
}
