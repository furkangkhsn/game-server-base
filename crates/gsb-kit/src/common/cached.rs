//! [`Cached`] — a room's world query, built once and kept across ticks
//! (KIT-ARCHITECTURE §10 "A12").
//!
//! `World::query_filtered` builds a fresh `QueryState` on every call:
//! component lookups, the access sets and a scan of every archetype
//! holding the rarest required component. A kit room ran several of
//! those per tick (the record pass, the orphan stamp, the dirty pass,
//! the border export, the migration scan); the state is now kept in the
//! room and only iterated.
//!
//! **Byte identity.** A kept `QueryState` learns new archetypes
//! incrementally, APPENDING them to its storage list — while a fresh one
//! lists every archetype in the world's component-index order, which is
//! a hash order, not creation order. The two can iterate the same
//! entities in a different order, and a room's records go on the wire in
//! query order. So the state is rebuilt whenever the world's archetype
//! generation moved: between two builds the archetype set is the same,
//! and the kept state iterates exactly as a fresh one would — the frames
//! stay byte for byte what they were. In a steady world (no new
//! component combination) that is never; new entities in an archetype
//! the state already knows are seen without a rebuild.
//!
//! The world's identity is kept too: a state is only valid for the world
//! it was built from (bevy panics otherwise), and a room's logic is not
//! bound to one world by its type.

use bevy_ecs::archetype::ArchetypeGeneration;
use bevy_ecs::prelude::{Entity, With, Without, World};
use bevy_ecs::query::{QueryData, QueryFilter, QueryState};
use bevy_ecs::world::WorldId;

use crate::codec::RecordCodec;
use crate::identity::WireId;

/// A world query kept across ticks (module docs).
pub(crate) struct Cached<D: QueryData + 'static, F: QueryFilter + 'static = ()> {
    /// The state, with the world it was built from and that world's
    /// archetype generation at the build.
    state: Option<(WorldId, ArchetypeGeneration, QueryState<D, F>)>,
}

impl<D: QueryData + 'static, F: QueryFilter + 'static> Default for Cached<D, F> {
    fn default() -> Self {
        Self { state: None }
    }
}

impl<D: QueryData + 'static, F: QueryFilter + 'static> Cached<D, F> {
    /// The query's state for `world`: the kept one while `world` is the
    /// world it was built from and no archetype was created since, else
    /// a fresh one (kept from now on). Iterate it with `iter` /
    /// `iter_mut` (they update the archetypes themselves — a no-op here).
    pub(crate) fn state(&mut self, world: &mut World) -> &mut QueryState<D, F> {
        let id = world.id();
        let generation = world.archetypes().generation();
        let kept = matches!(&self.state, Some((w, g, _)) if *w == id && *g == generation);
        if !kept {
            self.state = None;
        }
        let (_, _, state) = self.state.get_or_insert_with(|| {
            let state = world.query_filtered::<D, F>();
            // Read after the build: building registers components, never
            // archetypes — but the generation kept is the one the state
            // saw, whatever the build did.
            (id, world.archetypes().generation(), state)
        });
        state
    }
}

/// The orphan query: broadcastable (`M`, the codec's marker) and not yet
/// stamped with a wire identity.
pub(crate) type Orphans<M> = Cached<Entity, (With<M>, Without<WireId>)>;

/// The record query of codec `C`: every broadcastable entity's wire
/// identity and record components.
pub(crate) type RecordPass<C> =
    Cached<(&'static WireId, <C as RecordCodec>::Query), With<<C as RecordCodec>::Marker>>;

#[cfg(test)]
mod tests;
