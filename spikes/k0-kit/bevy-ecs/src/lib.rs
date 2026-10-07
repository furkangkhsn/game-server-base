//! k0 spike — the bevy_ecs binding of the agnostic cell core
//! (`docs/KIT-ECS.md` §5.3). Everything bevy-shaped the kit's AOI path
//! uses today lives here and nowhere else:
//!
//! - [`KitWire`]: the wire id as a bevy component (the core's `WireId`
//!   has no `Component` derive; the orphan rule forbids deriving it
//!   here, so the binding wraps it);
//! - [`OwnerExt`] / [`EntityExt`]: `Entity` ⇄ [`Owner`] (SERVICES §5.5's
//!   adapter idea);
//! - [`BevyCellFeed`]: `Changed<T>` + `With<M>` through a kept
//!   `QueryState` (A12's `Cached`), the removed-component buffers, and
//!   the one `clear_trackers` call.

use std::collections::HashMap;
use std::marker::PhantomData;

use bevy_ecs::archetype::ArchetypeGeneration;
use bevy_ecs::component::Component;
use bevy_ecs::prelude::{Changed, Entity, With, World};
use bevy_ecs::query::{QueryData, QueryFilter, QueryState};
use bevy_ecs::world::WorldId;
use k0_kit_core::{CellFeed, Gone, Owner, ToWire, WireId};

/// The wire id on a bevy entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct KitWire(pub WireId);

/// `Entity → Owner`.
pub trait EntityExt {
    /// The owner id of this entity.
    fn to_owner(self) -> Owner;
}

impl EntityExt for Entity {
    #[inline]
    fn to_owner(self) -> Owner {
        Owner(self.to_bits())
    }
}

/// `Owner → Entity` (only for owners this binding made).
pub trait OwnerExt {
    /// The entity behind this owner id.
    fn entity(self) -> Entity;
}

impl OwnerExt for Owner {
    #[inline]
    fn entity(self) -> Entity {
        Entity::from_bits(self.0)
    }
}

/// A world query kept across ticks — `gsb-kit`'s `common/cached.rs`,
/// moved here (it is a bevy-only optimisation). The archetype-generation
/// rebuild is kept for parity with today's cost; with the core's
/// canonical order it is no longer needed for byte identity.
pub struct Cached<D: QueryData + 'static, F: QueryFilter + 'static = ()> {
    state: Option<(WorldId, ArchetypeGeneration, QueryState<D, F>)>,
}

impl<D: QueryData + 'static, F: QueryFilter + 'static> Default for Cached<D, F> {
    fn default() -> Self {
        Self { state: None }
    }
}

impl<D: QueryData + 'static, F: QueryFilter + 'static> Cached<D, F> {
    /// The kept state for `world` (rebuilt when the archetypes moved).
    pub fn state(&mut self, world: &mut World) -> &mut QueryState<D, F> {
        let id = world.id();
        let generation = world.archetypes().generation();
        let kept = matches!(&self.state, Some((w, g, _)) if *w == id && *g == generation);
        if !kept {
            self.state = None;
        }
        let (_, _, state) = self.state.get_or_insert_with(|| {
            let state = world.query_filtered::<D, F>();
            (id, world.archetypes().generation(), state)
        });
        state
    }
}

/// The dirty query of [`BevyCellFeed`]: changed, broadcastable records.
pub type DirtyPass<T, M> = Cached<(Entity, &'static KitWire, &'static T), (Changed<T>, With<M>)>;

/// The bevy feed: records are entities with the marker `M` and the
/// record component `T`; a write is any `&mut T` deref (bevy's own
/// change detection — structural, every writer).
pub struct BevyCellFeed<T: Component + ToWire, M: Component> {
    dirty: DirtyPass<T, M>,
    _marker: PhantomData<fn() -> M>,
}

impl<T: Component + ToWire, M: Component> Default for BevyCellFeed<T, M> {
    fn default() -> Self {
        Self {
            dirty: Cached::default(),
            _marker: PhantomData,
        }
    }
}

impl<T: Component + ToWire, M: Component> CellFeed<T::Wire> for BevyCellFeed<T, M> {
    type World = World;

    #[inline]
    fn changed(&mut self, world: &mut World, mut sink: impl FnMut(Owner, u64, T::Wire)) {
        for (entity, wire, item) in self.dirty.state(world).iter(world) {
            sink(entity.to_owner(), wire.0.get(), item.wire());
        }
    }

    fn gone<C>(
        &mut self,
        world: &World,
        known: &HashMap<Owner, (u64, C)>,
        mut sink: impl FnMut(Gone),
    ) {
        // `gsb-kit`'s `sweep_removed`, verbatim in substance.
        for entity in world.removed::<KitWire>().chain(world.removed::<M>()) {
            let owner = entity.to_owner();
            let Some(&(wire, _)) = known.get(&owner) else {
                continue;
            };
            let alive = world.get_entity(entity).ok();
            let still_placed = alive.is_some_and(|e| {
                e.contains::<M>() && e.get::<KitWire>().is_some_and(|w| w.0.get() == wire)
            });
            if !still_placed {
                sink(Gone {
                    owner,
                    alive: alive.is_some(),
                });
            }
        }
    }

    #[inline]
    fn close(&mut self, world: &mut World) {
        world.clear_trackers();
    }
}
