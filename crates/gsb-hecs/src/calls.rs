//! The owner model of the call table in a hecs world
//! (`docs/SERVICES.md` §5.5).
//!
//! ```ignore
//! use gsb_hecs::{EntityExt, OwnerExt};
//!
//! fn transfer(world: &mut hecs::World, calls: &mut Calls, bank: &BankClient) {
//!     for r in calls.take_incoming::<TransferGold>() {
//!         let Some(e) = r.owner.entity() else { continue };
//!         let Ok(mut w) = world.get::<&mut Wallet>(e) else { continue };
//!         w.reserved += r.body.amount;
//!         calls.call(bank.charge(r.body.amount)).owner(r.owner).with(Reserve(r.body.amount)).answers(r.reply);
//!     }
//! }
//!
//! // GameLogic<hecs::World>:
//! fn call_owner(&self, _: &World, p: PlayerId) -> Option<Owner> { self.players.get(&p).map(|e| e.to_owner()) }
//! fn owner_alive(&self, world: &World, o: Owner) -> Option<bool> { Some(gsb_hecs::owner_alive(world, o)) }
//! ```
//!
//! (shipyard would be the same few lines — `EntityId::inner` ⇄
//! `EntityId::from_inner` — and needs no crate until a game asks for it.)

use gsb_core::Owner;
use hecs::{Entity, World};

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
        Entity::from_bits(self.to_bits())
    }
}

/// `e.to_owner()`: an entity as the owner of a call, a session or kit
/// state (`Entity::to_bits` — id and generation).
pub trait EntityExt {
    /// This entity as an [`Owner`].
    fn to_owner(self) -> Owner;
}

impl EntityExt for Entity {
    fn to_owner(self) -> Owner {
        Owner::from_bits(self.to_bits().get())
    }
}

/// Does `owner` still exist in `world` (`GameLogic::owner_alive` of a
/// game whose owners are entities)? `false` for an owner whose bits are
/// no entity's.
pub fn owner_alive(world: &World, owner: Owner) -> bool {
    owner.entity().is_some_and(|e| world.contains(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entity_round_trips_through_its_owner() {
        let mut world = World::new();
        let e = world.spawn((1u32,));
        let owner = e.to_owner();
        assert_eq!(owner.entity(), Some(e));
        assert!(owner_alive(&world, owner));
        world.despawn(e).expect("alive");
        assert!(!owner_alive(&world, owner), "a despawned owner is gone");
        let again = world.spawn((2u32,));
        assert_ne!(again.to_owner(), owner, "a recycled id is a new owner");
    }

    #[test]
    fn the_actor_and_a_zero_owner_are_no_entity() {
        assert_eq!(Owner(0).entity(), None);
        assert_eq!(Owner::ACTOR.entity(), None);
        assert!(!owner_alive(&World::new(), Owner(0)));
    }
}
