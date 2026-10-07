//! k0 spike — the hecs binding of the agnostic cell core
//! (`docs/KIT-ECS.md` §5.4, §6).
//!
//! hecs has neither change ticks nor removal events. The binding
//! supplies both, without a discipline the game must remember:
//!
//! - **Change detection — [`Tracked<T>`]**: the record component wrapped
//!   in a flag that its `DerefMut` sets — the same rule as bevy's
//!   `Mut<T>` (every `&mut` access marks, even a same-value write), so
//!   every writer marks by construction. [`HecsTrackedFeed`] reads and
//!   clears the flags in one linear pass over the archetype columns —
//!   the same O(N) shape as bevy's `Changed<T>` filter, which also
//!   checks a tick per entity.
//! - **Removal — the census**: the feed counts the records it scanned;
//!   when the count equals the book's known records, nothing left and
//!   the sweep is skipped. Otherwise each known owner is checked against
//!   a dense per-entity-index stamp written during the scan (an array
//!   read, no archetype lookup).
//!
//! [`HecsDiffFeed`] is the fallback with no change source at all: it
//! reports EVERY record every tick and lets the book's wire compare drop
//! the unchanged ones (one hash lookup per record) — measured to show
//! what `Tracked<T>` buys.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use hecs::{Entity, World};
use k0_kit_core::{CellFeed, Gone, Owner, ToWire, WireId};

/// `Entity → Owner`.
pub trait EntityExt {
    /// The owner id of this entity.
    fn to_owner(self) -> Owner;
}

impl EntityExt for Entity {
    #[inline]
    fn to_owner(self) -> Owner {
        Owner(self.to_bits().get())
    }
}

/// `Owner → Entity` (only for owners this binding made).
pub trait OwnerExt {
    /// The entity behind this owner id.
    fn entity(self) -> Option<Entity>;
}

impl OwnerExt for Owner {
    #[inline]
    fn entity(self) -> Option<Entity> {
        Entity::from_bits(self.0)
    }
}

/// A record component whose every mutable access marks it changed (the
/// hecs binding's change source). A fresh value starts changed (a spawn
/// is a write, as in bevy).
#[derive(Debug, Clone)]
pub struct Tracked<T> {
    value: T,
    changed: bool,
}

impl<T> Tracked<T> {
    /// A fresh tracked value (marked changed).
    pub fn new(value: T) -> Self {
        Self {
            value,
            changed: true,
        }
    }
}

impl<T> Deref for Tracked<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Tracked<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        self.changed = true;
        &mut self.value
    }
}

/// The census both hecs feeds keep: records scanned this tick, and a
/// dense `entity index → (tick, owner bits)` stamp.
#[derive(Default)]
struct Census {
    tick: u64,
    scanned: usize,
    seen: Vec<(u64, u64)>,
}

impl Census {
    #[inline]
    fn begin(&mut self) {
        self.tick += 1;
        self.scanned = 0;
    }

    #[inline]
    fn saw(&mut self, entity: Entity) {
        self.scanned += 1;
        let i = entity.id() as usize;
        if i >= self.seen.len() {
            self.seen.resize(i + 1, (0, 0));
        }
        self.seen[i] = (self.tick, entity.to_bits().get());
    }

    fn gone<C>(&self, world: &World, known: &HashMap<Owner, (u64, C)>, mut sink: impl FnMut(Gone)) {
        if known.len() == self.scanned {
            return; // every known record was scanned: nothing left
        }
        for &owner in known.keys() {
            let Some(entity) = owner.entity() else {
                continue;
            };
            let here = self
                .seen
                .get(entity.id() as usize)
                .is_some_and(|&s| s == (self.tick, owner.0));
            if !here {
                sink(Gone {
                    owner,
                    alive: world.contains(entity),
                });
            }
        }
    }
}

/// The hecs feed over [`Tracked<T>`] records carrying a [`WireId`].
pub struct HecsTrackedFeed<T> {
    census: Census,
    _t: PhantomData<fn() -> T>,
}

impl<T> Default for HecsTrackedFeed<T> {
    fn default() -> Self {
        Self {
            census: Census::default(),
            _t: PhantomData,
        }
    }
}

impl<T: ToWire + Send + Sync + 'static> CellFeed<T::Wire> for HecsTrackedFeed<T> {
    type World = World;

    #[inline]
    fn changed(&mut self, world: &mut World, mut sink: impl FnMut(Owner, u64, T::Wire)) {
        self.census.begin();
        for (entity, (wire, rec)) in world.query_mut::<(&WireId, &mut Tracked<T>)>() {
            self.census.saw(entity);
            if rec.changed {
                rec.changed = false;
                sink(entity.to_owner(), wire.get(), rec.value.wire());
            }
        }
    }

    fn gone<C>(&mut self, world: &World, known: &HashMap<Owner, (u64, C)>, sink: impl FnMut(Gone)) {
        self.census.gone(world, known, sink);
    }

    /// The flags were cleared while read: nothing to close.
    #[inline]
    fn close(&mut self, _world: &mut World) {}
}

/// The hecs feed with no change source: every record, every tick.
pub struct HecsDiffFeed<T> {
    census: Census,
    _t: PhantomData<fn() -> T>,
}

impl<T> Default for HecsDiffFeed<T> {
    fn default() -> Self {
        Self {
            census: Census::default(),
            _t: PhantomData,
        }
    }
}

impl<T: ToWire + Send + Sync + 'static> CellFeed<T::Wire> for HecsDiffFeed<T> {
    type World = World;

    #[inline]
    fn changed(&mut self, world: &mut World, mut sink: impl FnMut(Owner, u64, T::Wire)) {
        self.census.begin();
        for (entity, (wire, rec)) in world.query_mut::<(&WireId, &T)>() {
            self.census.saw(entity);
            sink(entity.to_owner(), wire.get(), rec.wire());
        }
    }

    fn gone<C>(&mut self, world: &World, known: &HashMap<Owner, (u64, C)>, sink: impl FnMut(Gone)) {
        self.census.gone(world, known, sink);
    }

    #[inline]
    fn close(&mut self, _world: &mut World) {}
}
