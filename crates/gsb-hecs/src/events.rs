//! The event table in a hecs world (`docs/EVENTS.md` §4.5). A hecs
//! game's systems are plain functions that take `&mut Events` next to
//! the world (`fn gold(w: &mut hecs::World, events: &mut Events)`), so
//! there is nothing to lend; this module adds the one loop every such
//! system writes — an event to the component of the entity that owns it.
//!
//! ```ignore
//! use gsb_hecs::events::take_for;
//!
//! fn gold(w: &mut hecs::World, events: &mut Events) {
//!     let rest = take_for::<GoldReceived, Wallet>(w, events, |wallet, e| wallet.apply(e.body.balance, e.body.version));
//!     for e in rest { /* room-wide, or an owner without a wallet: the game decides */ }
//! }
//!
//! // GameLogic<hecs::World>:
//! fn boot_events(&mut self, events: &mut Events) -> Result<(), BootError> { events.accept::<GoldReceived>() }
//! fn update_with_events(&mut self, world: &mut World, _: &TickCtx, calls: &mut Calls, events: &mut Events) {
//!     gold(world, events);
//!     transfers(world, calls, &self.bank);
//! }
//! ```

use gsb_core::events::{Arrived, Events, WorldEvent};
use hecs::{Component, World};

use crate::OwnerExt;

/// Take this tick's `E` events (arrival order) and hand each one whose
/// owner is a live entity holding a `C` to `f`, with that component;
/// return the others — room-wide events (their owner is
/// [`Owner::ACTOR`](gsb_core::Owner::ACTOR)), owners that are no
/// entity's, gone, or without a `C` — in order, for the game to decide:
/// nothing vanishes in the helper (and what the game drops it drops
/// knowingly; the table counted them all `events_taken`).
pub fn take_for<E: WorldEvent, C: Component>(
    world: &mut World,
    events: &mut Events,
    mut f: impl FnMut(&mut C, Arrived<E>),
) -> Vec<Arrived<E>> {
    let mut rest = Vec::new();
    for e in events.take::<E>() {
        match e.owner.entity().and_then(|x| world.get::<&mut C>(x).ok()) {
            Some(mut c) => f(&mut c, e),
            None => rest.push(e),
        }
    }
    rest
}
