//! The cell book's optional `wire id → entity` index (KIT-ARCHITECTURE
//! §10 "A9"): the lit AOI room asks the game about a record's ENTITY,
//! while the buckets hold wire ids. Kept only by a room that asks for it
//! — every other room pays nothing (the field stays `None`); the dirty
//! pass and the two removal passes write it where `last_cell` gains or
//! loses the entity.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

use bevy_ecs::prelude::Entity;

use super::CellBook;

impl<W: Clone + Eq, C: Copy + Eq + Hash + Debug> CellBook<W, C> {
    /// Keep the index from now on, starting from what is bucketed
    /// already (the lit room asks at construction — nothing yet).
    pub(crate) fn index_entities(&mut self) {
        let index = self.entities.get_or_insert_with(HashMap::new);
        for (&entity, &(wire, _)) in &self.last_cell {
            index.insert(wire, entity);
        }
    }

    /// The entity of bucketed record `wire`, when the index is kept.
    #[inline]
    pub(crate) fn entity_of(&self, wire: u64) -> Option<Entity> {
        self.entities.as_ref()?.get(&wire).copied()
    }
}
