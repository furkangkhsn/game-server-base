//! The call table in a bevy_ecs world (`docs/SERVICES.md` §5.5).
//!
//! ```ignore
//! use gsb_bevy_ecs::{EntityExt, GsbCalls, OwnerExt, Svc};
//!
//! fn transfer(mut calls: ResMut<GsbCalls>, bank: Res<Svc<BankClient>>, mut q: Query<&mut Wallet>) {
//!     for r in calls.take_incoming::<TransferGold>() {
//!         let Some(mut w) = r.owner.entity().and_then(|e| q.get_mut(e).ok()) else { continue };
//!         w.reserved += r.body.amount;
//!         calls.call(bank.charge(r.body.amount)).owner(r.owner).with(Reserve(r.body.amount)).answers(r.reply);
//!     }
//! }
//!
//! // GameLogic<World>:
//! fn call_owner(&self, world: &World, p: PlayerId) -> Option<Owner> { self.players.get(&p).map(|e| e.to_owner()) }
//! fn owner_alive(&self, world: &World, o: Owner) -> Option<bool> { Some(gsb_bevy_ecs::owner_alive(world, o)) }
//! fn update_with_calls(&mut self, world: &mut World, _: &TickCtx, calls: &mut Calls) {
//!     gsb_bevy_ecs::lend(world, calls, |w| self.schedule.run(w));
//! }
//! ```
//!
//! Why a newtype and extension traits (not `impl Resource for Calls` or
//! `From<Owner> for Entity`): here `Calls`, `Resource` and `Entity` are
//! all foreign types — the orphan rule forbids both impls; the newtype and
//! local traits are legal and need no feature flag in the engine.

use std::ops::{Deref, DerefMut};

use bevy_ecs::entity::Entity;
use bevy_ecs::resource::Resource;
use bevy_ecs::world::World;
use gsb_core::Owner;
use gsb_core::calls::Calls;

/// The actor's call table as a bevy resource: systems take
/// `ResMut<GsbCalls>` and call `calls.call(..)`, `take_incoming`,
/// `take_resolved` as on `&mut Calls` (`Deref`/`DerefMut`).
///
/// The table belongs to the actor, which lends it each tick; [`lend`]
/// swaps it into this resource for the schedule's run and back out after.
/// Between runs the resource holds a spare, empty table.
#[derive(Resource, Debug)]
pub struct GsbCalls(pub Calls);

impl Deref for GsbCalls {
    type Target = Calls;
    fn deref(&self) -> &Calls {
        &self.0
    }
}

impl DerefMut for GsbCalls {
    fn deref_mut(&mut self) -> &mut Calls {
        &mut self.0
    }
}

/// A service client as a resource: `Res<Svc<InventoryClient>>`, the
/// client's methods through `Deref` (`inv.load(who)`).
#[derive(Resource, Debug, Clone)]
pub struct Svc<T: Send + Sync + 'static>(pub T);

impl<T: Send + Sync + 'static> Deref for Svc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// `owner.entity()`: the entity an owner names; `None` for
/// [`Owner::ACTOR`] and for bits that are no entity's. (A player-id
/// owner may decode to SOME entity's bits: a game mixing the two kinds
/// of owner tells them apart itself.)
pub trait OwnerExt {
    /// The entity behind this owner, if its bits are an entity's.
    fn entity(self) -> Option<Entity>;
}

impl OwnerExt for Owner {
    fn entity(self) -> Option<Entity> {
        // The actor's own owner is never an entity, whatever its bits
        // would decode to.
        if self == Owner::ACTOR {
            return None;
        }
        Entity::try_from_bits(self.to_bits())
    }
}

/// `e.to_owner()`: an entity as the owner of a call, a session or kit
/// state (`Entity::to_bits` — index and generation, so a recycled index
/// is a different owner).
pub trait EntityExt {
    /// This entity as an [`Owner`].
    fn to_owner(self) -> Owner;
}

impl EntityExt for Entity {
    fn to_owner(self) -> Owner {
        Owner::from_bits(self.to_bits())
    }
}

/// Does `owner` still exist in `world` (`GameLogic::owner_alive` of a
/// game whose owners are entities)? `false` for an owner whose bits are
/// no entity's.
pub fn owner_alive(world: &World, owner: Owner) -> bool {
    owner.entity().is_some_and(|e| world.get_entity(e).is_ok())
}

/// Run `f` (the game's schedule) with the actor's table in the world as
/// [`GsbCalls`]: the table is swapped into the resource before and back
/// out after — two moves, no allocation after the first call (which
/// inserts the resource with a spare table). Run the schedule on a
/// single-threaded executor: every system that issues calls takes
/// `ResMut<GsbCalls>` anyway, and the call order is the issue order.
pub fn lend<T>(world: &mut World, calls: &mut Calls, f: impl FnOnce(&mut World) -> T) -> T {
    if !world.contains_resource::<GsbCalls>() {
        world.insert_resource(GsbCalls(Calls::default()));
    }
    std::mem::swap(&mut world.resource_mut::<GsbCalls>().0, calls);
    let out = f(world);
    let mut slot = world
        .get_resource_mut::<GsbCalls>()
        .expect("the game's schedule removed the GsbCalls resource it was lent");
    std::mem::swap(&mut slot.0, calls);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::prelude::{Component, ResMut, Schedule};

    #[derive(Component)]
    struct Marker;

    #[test]
    fn an_entity_round_trips_through_its_owner() {
        let mut world = World::new();
        let e = world.spawn(Marker).id();
        let owner = e.to_owner();
        assert_eq!(owner.to_bits(), e.to_bits());
        assert_eq!(owner.entity(), Some(e));
        assert!(owner_alive(&world, owner));
        world.despawn(e);
        assert!(!owner_alive(&world, owner), "a despawned owner is gone");
        let again = world.spawn(Marker).id();
        assert_ne!(again.to_owner(), owner, "a recycled index is a new owner");
    }

    #[test]
    fn the_actor_owner_is_no_entity() {
        assert_eq!(Owner::ACTOR.entity(), None);
        assert!(!owner_alive(&World::new(), Owner::ACTOR));
    }

    /// The schedule sees the actor's own table (its config), and the
    /// actor has it back afterwards — every run.
    #[test]
    fn lend_hands_the_actors_table_to_the_schedule_and_back() {
        let mut world = World::new();
        let mut table = Calls::new(gsb_core::calls::CallsConfig {
            share: 7,
            ..Default::default()
        });
        let mut schedule = Schedule::default();
        schedule.add_systems(|mut calls: ResMut<GsbCalls>| {
            assert_eq!(calls.config().share, 7, "the lent table, not the spare");
            calls.bind(gsb_core::ConnectionId(1), Owner(9));
        });
        for _ in 0..2 {
            lend(&mut world, &mut table, |w| schedule.run(w));
        }
        assert_eq!(table.bound(gsb_core::ConnectionId(1)), Some(Owner(9)));
        assert_eq!(table.config().share, 7, "the actor's table is back");
        assert_ne!(
            world.resource::<GsbCalls>().config().share,
            7,
            "the spare stays"
        );
    }

    #[test]
    fn a_client_resource_derefs_to_the_client() {
        let svc = Svc(String::from("bank"));
        assert_eq!(svc.len(), 4);
    }
}
