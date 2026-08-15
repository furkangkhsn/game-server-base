//! Game systems of the demo game.
//!
//! A [`gsb_ecs::System`] is a plain, ordered pass over the room's world.
//! The room runs them sequentially, single-threaded — the room actor is the
//! only owner of the world, so no synchronization is ever needed.

use bevy_ecs::prelude::Entity;
use bevy_ecs::world::World;
use gsb_ecs::dirty::bump;
use gsb_ecs::{System, SystemCtx};

use crate::components::{DEFAULT_SPEED, MoveTarget, Position, Speed};

/// Moves entities toward their [`MoveTarget`]; bumps the entity version on
/// every tick in which an entity actually moved (so the broadcast phase can
/// pick it up).
///
/// Two passes by design: first collect the writes while the query iterator
/// holds the world borrow, then apply them. This keeps the hot path free of
/// per-entity `World` lookups and is the idiomatic bevy pattern.
/// Pending write computed during the read pass.
struct Step {
    entity: Entity,
    x: f32,
    y: f32,
    arrived: bool,
}

pub struct MovementSystem;

impl System for MovementSystem {
    fn run(&mut self, world: &mut World, ctx: &SystemCtx) {
        let dt = ctx.dt;
        if dt <= 0.0 {
            return;
        }

        let mut query = world.query::<(Entity, &Position, &MoveTarget, Option<&Speed>)>();
        let mut steps: Vec<Step> = Vec::new();
        for (entity, pos, target, speed) in query.iter(world) {
            let speed = speed.map(|s| s.0).unwrap_or(DEFAULT_SPEED);
            let dx = target.x - pos.x;
            let dy = target.y - pos.y;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= f32::EPSILON {
                continue; // already on target
            }
            let step = speed * dt;
            if dist <= step {
                steps.push(Step {
                    entity,
                    x: target.x,
                    y: target.y,
                    arrived: true,
                });
            } else {
                let t = step / dist;
                steps.push(Step {
                    entity,
                    x: pos.x + dx * t,
                    y: pos.y + dy * t,
                    arrived: false,
                });
            }
        }

        for step in steps {
            let Step {
                entity,
                x,
                y,
                arrived,
            } = step;
            {
                let mut e = world.entity_mut(entity);
                *e.get_mut::<Position>().expect("queried above") = Position { x, y };
                if arrived {
                    e.remove::<MoveTarget>();
                }
            }
            bump(world, entity);
        }
    }
}
