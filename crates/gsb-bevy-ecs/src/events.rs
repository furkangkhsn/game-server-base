//! The event table in a bevy_ecs world (`docs/EVENTS.md` §4.5): the
//! twin of [`crate::calls`].
//!
//! ```ignore
//! use gsb_bevy_ecs::{GsbEvents, OwnerExt};
//!
//! fn gold(mut events: ResMut<GsbEvents>, mut q: Query<&mut Wallet>) {
//!     for e in events.take::<GoldReceived>() {
//!         if let Some(mut w) = e.owner.entity().and_then(|x| q.get_mut(x).ok()) {
//!             w.apply(e.body.balance, e.body.version);
//!         }
//!     }
//! }
//!
//! // GameLogic<World>:
//! fn boot_events(&mut self, events: &mut Events) -> Result<(), BootError> { events.accept::<GoldReceived>() }
//! fn update_with_events(&mut self, world: &mut World, _: &TickCtx, calls: &mut Calls, events: &mut Events) {
//!     gsb_bevy_ecs::events::lend(world, events, |w| gsb_bevy_ecs::lend(w, calls, |w| self.schedule.run(w)));
//! }
//! ```
//!
//! An event's owner is the call table's owner (the session's explicit
//! bind, else `GameLogic::call_owner`): a game whose owners are entities
//! reads `e.owner.entity()`; a room-wide event's owner is
//! [`Owner::ACTOR`](gsb_core::Owner::ACTOR), never an entity.

use std::ops::{Deref, DerefMut};

use bevy_ecs::resource::Resource;
use bevy_ecs::world::World;
use gsb_core::events::Events;

/// The actor's event table as a bevy resource: systems take
/// `ResMut<GsbEvents>` and call `events.take::<E>()` / `take_all()` as
/// on `&mut Events` (`Deref`/`DerefMut`). [`lend`] swaps the actor's
/// table in for the schedule's run and back out after; between runs the
/// resource holds a spare, empty table (what a system takes from it then
/// is nothing).
#[derive(Resource, Debug, Default)]
pub struct GsbEvents(pub Events);

impl Deref for GsbEvents {
    type Target = Events;
    fn deref(&self) -> &Events {
        &self.0
    }
}

impl DerefMut for GsbEvents {
    fn deref_mut(&mut self) -> &mut Events {
        &mut self.0
    }
}

/// Run `f` (the game's schedule) with the actor's event table in the
/// world as [`GsbEvents`]: two moves, no allocation after the first call
/// (which inserts the resource with a spare table). Nest it with
/// [`crate::lend`] to lend both tables. What the systems leave untaken
/// goes back with the table: the actor drops and counts it at the tick's
/// end (`events_untaken`).
pub fn lend<T>(world: &mut World, events: &mut Events, f: impl FnOnce(&mut World) -> T) -> T {
    if !world.contains_resource::<GsbEvents>() {
        world.insert_resource(GsbEvents::default());
    }
    std::mem::swap(&mut world.resource_mut::<GsbEvents>().0, events);
    let out = f(world);
    let mut slot = world
        .get_resource_mut::<GsbEvents>()
        .expect("the game's schedule removed the GsbEvents resource it was lent");
    std::mem::swap(&mut slot.0, events);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::prelude::{ResMut, Schedule};
    use gsb_core::events::Event;

    /// A one-byte event (no codegen in this crate's tests).
    #[derive(Debug, Clone, PartialEq)]
    struct Ping(u8);
    impl gsb_core::calls::WireMessage for Ping {
        fn to_wire(&self) -> Vec<u8> {
            vec![self.0]
        }
        fn from_wire(b: &[u8]) -> Result<Self, gsb_core::calls::DecodeError> {
            b.first()
                .map(|v| Ping(*v))
                .ok_or(gsb_core::calls::DecodeError)
        }
    }
    impl gsb_core::events::Event for Ping {
        const OP: u16 = 4100;
        const TO: gsb_core::events::Dest = gsb_core::events::Dest::World;
    }
    impl gsb_core::events::WorldEvent for Ping {}

    /// The schedule sees the actor's table (its registrations), and the
    /// actor has it back afterwards — every run; the spare stays.
    #[test]
    fn lend_hands_the_actors_table_to_the_schedule_and_back() {
        let mut world = World::new();
        let mut table = Events::default();
        table.accept::<Ping>().expect("free op");
        let mut schedule = Schedule::default();
        schedule.add_systems(|mut events: ResMut<GsbEvents>| {
            assert!(events.handles(Ping::OP), "the lent table, not the spare");
            assert!(events.take::<Ping>().is_empty(), "nothing arrived");
        });
        for _ in 0..2 {
            lend(&mut world, &mut table, |w| schedule.run(w));
        }
        assert!(table.handles(Ping::OP), "the actor's table is back");
        assert!(
            !world.resource::<GsbEvents>().handles(Ping::OP),
            "the spare stays"
        );
    }
}
