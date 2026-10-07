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
//!   the sweep is skipped. Otherwise one sequential pass over a dense
//!   per-entity-index stamp written during the scan finds the records
//!   seen last scan and not this one (no archetype lookup, no walk of
//!   the book's hash map).
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
struct Census {
    /// The scan counter (starts at 1: a fresh stamp `(0, 0)` is never
    /// "seen last scan").
    tick: u64,
    /// Records scanned this tick.
    scanned: usize,
    /// `entity index → (last scan that saw it, its entity bits)`: dense,
    /// written in the scan's own (archetype) order.
    seen: Vec<(u64, u64)>,
    /// Entities seen last scan whose index a NEW entity took this scan
    /// (hecs recycles indices; the stamp alone would hide the death).
    displaced: Vec<u64>,
}

impl Default for Census {
    fn default() -> Self {
        Self {
            tick: 1,
            scanned: 0,
            seen: Vec::new(),
            displaced: Vec::new(),
        }
    }
}

impl Census {
    #[inline]
    fn begin(&mut self) {
        self.tick += 1;
        self.scanned = 0;
        self.displaced.clear();
    }

    #[inline]
    fn saw(&mut self, entity: Entity) {
        self.scanned += 1;
        let i = entity.id() as usize;
        if i >= self.seen.len() {
            self.seen.resize(i + 1, (0, 0));
        }
        let bits = entity.to_bits().get();
        let (last, was) = self.seen[i];
        if last == self.tick - 1 && was != bits {
            self.displaced.push(was);
        }
        self.seen[i] = (self.tick, bits);
    }

    /// The records the book knows that this scan did not see: skipped
    /// outright when the census balances (no despawn this tick — the
    /// steady state); otherwise one SEQUENTIAL pass over the dense stamps
    /// (seen last scan, not this one) plus the displaced, each confirmed
    /// against `known` (a parked removal is already out of it).
    fn gone<C>(&self, world: &World, known: &HashMap<Owner, (u64, C)>, mut sink: impl FnMut(Gone)) {
        if known.len() == self.scanned {
            return; // every known record was scanned: nothing left
        }
        let prev = self.tick - 1;
        let stale = self
            .seen
            .iter()
            .filter(|&&(t, _)| t == prev)
            .map(|&(_, bits)| bits);
        for bits in stale.chain(self.displaced.iter().copied()) {
            let owner = Owner(bits);
            if !known.contains_key(&owner) {
                continue;
            }
            let alive = owner.entity().is_some_and(|e| world.contains(e));
            sink(Gone { owner, alive });
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

// ── The removal log: a world wrapper ───────────────────────────────────

/// A hecs world whose structural changes are LOGGED — the removal events
/// hecs lacks, made structural the way bevy's are: the wrapper derefs
/// only to `&hecs::World` (queries, `query::<&mut T>()` writes — a
/// `Tracked<T>` still flags them), and every call that can take a record
/// out of the broadcast set (`despawn`, `remove`, `remove_one`) goes
/// through it and is logged. A game holding `&mut KitWorld` has no other
/// path to a despawn. `query_mut` / `query_one_mut` (no structural
/// change) are passed through.
#[derive(Default)]
pub struct KitWorld {
    world: World,
    /// Entities a structural call touched since the last close.
    touched: Vec<Entity>,
}

impl Deref for KitWorld {
    type Target = World;
    #[inline]
    fn deref(&self) -> &World {
        &self.world
    }
}

impl KitWorld {
    /// Spawn (no log: a spawn is seen by the change flags).
    pub fn spawn(&mut self, components: impl hecs::DynamicBundle) -> Entity {
        self.world.spawn(components)
    }

    /// Despawn, logged.
    pub fn despawn(&mut self, entity: Entity) -> Result<(), hecs::NoSuchEntity> {
        self.touched.push(entity);
        self.world.despawn(entity)
    }

    /// Insert one component (an insert cannot take a record out).
    pub fn insert_one(
        &mut self,
        entity: Entity,
        component: impl hecs::Component,
    ) -> Result<(), hecs::NoSuchEntity> {
        self.world.insert_one(entity, component)
    }

    /// Remove one component, logged.
    pub fn remove_one<C: hecs::Component>(
        &mut self,
        entity: Entity,
    ) -> Result<C, hecs::ComponentError> {
        self.touched.push(entity);
        self.world.remove_one::<C>(entity)
    }

    /// The exclusive query (no structural change).
    pub fn query_mut<Q: hecs::Query>(&mut self) -> hecs::QueryMut<'_, Q> {
        self.world.query_mut::<Q>()
    }

    /// The exclusive one-entity query (no structural change).
    pub fn query_one_mut<Q: hecs::Query>(
        &mut self,
        entity: Entity,
    ) -> Result<Q::Item<'_>, hecs::QueryOneError> {
        self.world.query_one_mut::<Q>(entity)
    }
}

/// The hecs feed over a [`KitWorld`]: `Tracked<T>` flags for changes,
/// the wrapper's log for removals — O(removals), no census.
pub struct HecsLoggedFeed<T> {
    _t: PhantomData<fn() -> T>,
}

impl<T> Default for HecsLoggedFeed<T> {
    fn default() -> Self {
        Self { _t: PhantomData }
    }
}

impl<T: ToWire + Send + Sync + 'static> CellFeed<T::Wire> for HecsLoggedFeed<T> {
    type World = KitWorld;

    #[inline]
    fn changed(&mut self, world: &mut KitWorld, mut sink: impl FnMut(Owner, u64, T::Wire)) {
        for (entity, (wire, rec)) in world.world.query_mut::<(&WireId, &mut Tracked<T>)>() {
            if rec.changed {
                rec.changed = false;
                sink(entity.to_owner(), wire.get(), rec.value.wire());
            }
        }
    }

    fn gone<C>(
        &mut self,
        world: &KitWorld,
        known: &HashMap<Owner, (u64, C)>,
        mut sink: impl FnMut(Gone),
    ) {
        for &entity in &world.touched {
            let owner = entity.to_owner();
            let Some(&(wire, _)) = known.get(&owner) else {
                continue;
            };
            let alive = world.world.contains(entity);
            let still_placed = alive
                && world.world.entity(entity).ok().is_some_and(|e| {
                    e.has::<Tracked<T>>() && e.get::<&WireId>().is_some_and(|w| w.get() == wire)
                });
            if !still_placed {
                sink(Gone { owner, alive });
            }
        }
    }

    /// The log is the window: cleared here, like bevy's buffers.
    #[inline]
    fn close(&mut self, world: &mut KitWorld) {
        world.touched.clear();
    }
}
