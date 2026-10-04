//! The three room flavours. Each is a few lines: where the table lives,
//! how "is this owner alive" is answered, and an owner ⇄ entity
//! extension (an extension trait, not `From`: in the engine these
//! adapters live outside the core crate, where `From<Owner> for Entity`
//! would break the orphan rule).

use crate::calls::Calls;
use crate::contract::Owner;
use crate::room::{Control, RoomLogic, StateHasher, TickCtx};

/// bevy: the table is a `Resource`; systems are ordinary function
/// systems taking `ResMut<Calls>`.
pub mod bevy {
    use bevy_ecs::prelude::*;

    use super::*;

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
            world.insert_resource(calls);
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
            self.world.resource_scope(|world, mut calls: Mut<Calls>| {
                let alive = |o: Owner| {
                    Entity::try_from_bits(o.0).is_some_and(|e| world.get_entity(e).is_ok())
                };
                f(&mut calls, &alive)
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
}

/// hecs: the table is a field next to the world; systems are functions
/// taking `(&mut World, &mut Calls, …)`.
pub mod hecs {
    use super::*;
    pub use ::hecs::{Entity, World};

    pub trait OwnerExt {
        fn entity(self) -> Option<Entity>;
    }
    impl OwnerExt for Owner {
        fn entity(self) -> Option<Entity> {
            Entity::from_bits(self.0)
        }
    }

    pub trait EntityExt {
        fn to_owner(self) -> Owner;
    }
    impl EntityExt for Entity {
        fn to_owner(self) -> Owner {
            Owner(self.to_bits().get())
        }
    }

    /// What a hecs game implements.
    pub trait HecsGame {
        fn control(&mut self, world: &mut World, c: &Control);
        fn systems(&mut self, world: &mut World, calls: &mut Calls, ctx: &mut TickCtx);
        fn state_hash(&self, world: &World, h: &mut StateHasher);
    }

    pub struct HecsRoom<G> {
        pub world: World,
        pub calls: Calls,
        pub game: G,
    }

    impl<G: HecsGame> RoomLogic for HecsRoom<G> {
        fn with_calls<T>(&mut self, f: impl FnOnce(&mut Calls, &dyn Fn(Owner) -> bool) -> T) -> T {
            let world = &self.world;
            f(&mut self.calls, &|o: Owner| {
                o.entity().is_some_and(|e| world.contains(e))
            })
        }
        fn control(&mut self, c: &Control, _ctx: &mut TickCtx) {
            self.game.control(&mut self.world, c);
        }
        fn systems(&mut self, ctx: &mut TickCtx) {
            self.game.systems(&mut self.world, &mut self.calls, ctx);
        }
        fn state_hash(&mut self, h: &mut StateHasher) {
            self.game.state_hash(&self.world, h);
        }
    }
}

/// No ECS: the game is a struct; it says itself which owners are alive.
pub mod plain {
    use super::*;

    pub trait PlainGame {
        fn alive(&self, owner: Owner) -> bool;
        fn control(&mut self, c: &Control);
        fn systems(&mut self, calls: &mut Calls, ctx: &mut TickCtx);
        fn state_hash(&self, h: &mut StateHasher);
    }

    pub struct PlainRoom<G> {
        pub calls: Calls,
        pub game: G,
    }

    impl<G: PlainGame> RoomLogic for PlainRoom<G> {
        fn with_calls<T>(&mut self, f: impl FnOnce(&mut Calls, &dyn Fn(Owner) -> bool) -> T) -> T {
            let game = &self.game;
            f(&mut self.calls, &|o| game.alive(o))
        }
        fn control(&mut self, c: &Control, _ctx: &mut TickCtx) {
            self.game.control(c);
        }
        fn systems(&mut self, ctx: &mut TickCtx) {
            self.game.systems(&mut self.calls, ctx);
        }
        fn state_hash(&mut self, h: &mut StateHasher) {
            self.game.state_hash(h);
        }
    }
}
