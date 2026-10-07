//! hecs adapter for the s0b `Calls` table (in the engine: an optional
//! crate such as `gsb-calls-hecs`): the owner ⇄ entity extension and a
//! room shape. The table is passed as `&mut Calls`; no wrapper needed.
//! (shipyard would be the same few lines: `EntityId` ⇄ `u64`.)

pub use hecs::{Entity, World};
use s0_calls::room::RoomLogic;
use s0_calls::{Calls, Control, Owner, StateHasher, TickCtx};

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
    /// Join hook: spawn the player and `calls.bind(conn, e.to_owner())`.
    fn control(&mut self, world: &mut World, calls: &mut Calls, c: &Control);
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
        self.game.control(&mut self.world, &mut self.calls, c);
    }
    fn systems(&mut self, ctx: &mut TickCtx) {
        self.game.systems(&mut self.world, &mut self.calls, ctx);
    }
    fn state_hash(&mut self, h: &mut StateHasher) {
        self.game.state_hash(&self.world, h);
    }
}
