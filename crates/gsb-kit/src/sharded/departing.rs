//! The migration tick (`docs/CROSS-SHARD.md` "D sonucu"): an entity
//! whose migration committed in tick `h` stays in this shard's world
//! until the core's migrate phase of `h + 1` despawns it — the DOOMED
//! COPY. Its state already travelled (captured in `h`); whatever the
//! game does to it in `h + 1` is lost, and whatever it does in `h + 1`
//! its new owner does too.
//!
//! So, for the game's hooks of `h + 1`, the copy is not this shard's
//! entity any more but its new owner's, seen from here: DISABLED in the
//! world (Bevy's default query filter — no game query finds it, so a
//! local hit misses it and its own systems do not run it) and LENT by
//! the new owner through the [`Seam`](super::Seam), with the record it
//! was captured with; an effect aimed at it is routed there by the core
//! and applied once, in order, like any cross-seam effect. The kit's own
//! passes after the game's systems (orphan stamping, crystallization,
//! the border export) see it again as before.
//!
//! Commit is the core's knowledge (a refused send leaves the entity
//! here): the copy is departing only while the core's
//! [`CrossSeam::departed`] names its new owner.

use std::collections::HashMap;

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::{Entity, World};
use gsb_core::shard::CrossSeam;

/// A sharded room's departing entities: the records captured by this
/// tick's migrate phase, and the copies hidden for the game's hooks of
/// the next tick. Bounded by one tick's migrations: an entry leaves
/// with its migration (`on_migrate_out`), with its entity, or — a
/// refused send — at the next tick's first hook.
#[derive(Debug)]
pub(in crate::sharded) struct Departures<V> {
    /// `wire → record` as the migrate phase captured it.
    captured: HashMap<u64, V>,
    /// The copies disabled for this tick's game hooks.
    hidden: Vec<Entity>,
}

impl<V> Default for Departures<V> {
    fn default() -> Self {
        Self {
            captured: HashMap::new(),
            hidden: Vec::new(),
        }
    }
}

impl<V> Departures<V> {
    /// `wire` is being handed on with `record` (the migrate phase
    /// reported it; the core may still refuse the send).
    pub(in crate::sharded) fn record(&mut self, wire: u64, record: V) {
        self.captured.insert(wire, record);
    }

    /// `wire` left (its copy despawned) or died here.
    pub(in crate::sharded) fn forget(&mut self, wire: u64) {
        self.captured.remove(&wire);
    }

    /// The new owner of `wire` and the record it left with, while its
    /// copy is still here.
    pub(in crate::sharded) fn departing<'s>(
        &'s self,
        wire: u64,
        cross: &CrossSeam<'_, V>,
    ) -> Option<(usize, &'s V)> {
        let record = self.captured.get(&wire)?;
        Some((cross.departed(wire)?, record))
    }

    /// Every departing wire with its new owner and record (unspecified
    /// order).
    pub(in crate::sharded) fn iter<'s>(
        &'s self,
        cross: &'s CrossSeam<'_, V>,
    ) -> impl Iterator<Item = (u64, usize, &'s V)> + 's {
        self.captured
            .iter()
            .filter_map(|(&wire, record)| Some((wire, cross.departed(wire)?, record)))
    }

    /// The copies hidden right now.
    pub(in crate::sharded) fn hidden(&self) -> &[Entity] {
        &self.hidden
    }

    /// Before the game's first hook of a tick: forget the refused sends
    /// and disable the copies whose migration committed (`own` names
    /// their entities). A no-op when already done this tick, or when
    /// nothing is departing.
    pub(in crate::sharded) fn hide(
        &mut self,
        world: &mut World,
        own: &HashMap<u64, Entity>,
        cross: &CrossSeam<'_, V>,
    ) {
        if self.captured.is_empty() || !self.hidden.is_empty() {
            return;
        }
        self.captured
            .retain(|&wire, _| cross.departed(wire).is_some());
        for wire in self.captured.keys() {
            let Some(&entity) = own.get(wire) else {
                continue;
            };
            if let Ok(mut copy) = world.get_entity_mut(entity) {
                copy.insert(Disabled);
                self.hidden.push(entity);
            }
        }
    }

    /// After the game's systems: the copies are back in the world for
    /// the kit's own passes (the core despawns them in its migrate
    /// phase).
    pub(in crate::sharded) fn show(&mut self, world: &mut World) {
        for entity in self.hidden.drain(..) {
            if let Ok(mut copy) = world.get_entity_mut(entity) {
                copy.remove::<Disabled>();
            }
        }
    }
}
