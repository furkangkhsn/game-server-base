//! bevy_ecs adapter for the s0b `Calls` table (in the engine: an optional
//! crate such as `gsb-calls-bevy-ecs`). Only the `bevy_ecs` crate — never
//! the Bevy engine. The core has no ECS dependency; a game that does not
//! use bevy_ecs never compiles this.
//!
//! - [`GsbCalls`]: the table as a `Resource` (newtype, `Deref`/`DerefMut`);
//! - [`OwnerExt`] / [`EntityExt`]: `owner.entity()`, `e.to_owner()`;
//! - [`BevyRoom`]: a world + schedule as a room (single-threaded executor:
//!   determinism).

use bevy_ecs::prelude::*;

use s0_calls::room::RoomLogic;
use s0_calls::{Calls, Control, Owner, StateHasher, TickCtx};

/// The table as a bevy resource: a newtype (the core has no ECS
/// dependency, and this crate may not implement bevy's `Resource` for the
/// core's `Calls` — the orphan rule). `Deref`/`DerefMut`, so systems
/// write `calls.call(..)` on a `ResMut<GsbCalls>` as on `&mut Calls`.
#[derive(Resource)]
pub struct GsbCalls(pub Calls);

impl std::ops::Deref for GsbCalls {
    type Target = Calls;
    fn deref(&self) -> &Calls {
        &self.0
    }
}

impl std::ops::DerefMut for GsbCalls {
    fn deref_mut(&mut self) -> &mut Calls {
        &mut self.0
    }
}

/// A generated client as a resource: `Res<Svc<InventoryClient>>`.
#[derive(Resource)]
pub struct Svc<T: Send + Sync + 'static>(pub T);

impl<T: Send + Sync + 'static> std::ops::Deref for Svc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// The tick context as a resource: `Res<Tick>`, `ResMut<Tick>` for
/// the rng (the room copies it in before and out after the schedule).
#[derive(Resource)]
pub struct Tick(pub TickCtx);

pub trait OwnerExt {
    fn entity(self) -> Entity;
}
impl OwnerExt for Owner {
    fn entity(self) -> Entity {
        Entity::try_from_bits(self.0).unwrap_or(Entity::PLACEHOLDER)
    }
}

pub trait EntityExt {
    fn to_owner(self) -> Owner;
}
impl EntityExt for Entity {
    fn to_owner(self) -> Owner {
        Owner(self.to_bits())
    }
}

/// A bevy room: a world, a schedule, and two plain functions.
pub struct BevyRoom {
    pub world: World,
    pub schedule: Schedule,
    control: fn(&mut World, &Control),
    hash: fn(&mut World, &mut StateHasher),
}

impl BevyRoom {
    pub fn new(
        calls: Calls,
        mut world: World,
        mut schedule: Schedule,
        control: fn(&mut World, &Control),
        hash: fn(&mut World, &mut StateHasher),
    ) -> Self {
        world.insert_resource(GsbCalls(calls));
        world.insert_resource(Tick(TickCtx::new(0)));
        schedule.set_executor(bevy_ecs::schedule::SingleThreadedExecutor::new());
        Self {
            world,
            schedule,
            control,
            hash,
        }
    }
}

impl RoomLogic for BevyRoom {
    fn with_calls<T>(&mut self, f: impl FnOnce(&mut Calls, &dyn Fn(Owner) -> bool) -> T) -> T {
        self.world
            .resource_scope(|world, mut calls: Mut<GsbCalls>| {
                let alive = |o: Owner| {
                    Entity::try_from_bits(o.0).is_some_and(|e| world.get_entity(e).is_ok())
                };
                f(&mut calls.0, &alive)
            })
    }
    fn control(&mut self, c: &Control, _ctx: &mut TickCtx) {
        (self.control)(&mut self.world, c);
    }
    fn systems(&mut self, ctx: &mut TickCtx) {
        self.world.resource_mut::<Tick>().0 = ctx.clone();
        self.schedule.run(&mut self.world);
        *ctx = self.world.resource::<Tick>().0.clone();
    }
    fn state_hash(&mut self, h: &mut StateHasher) {
        (self.hash)(&mut self.world, h);
    }
}
