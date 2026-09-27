//! The seam's merged queries (BACKLOG F6): every entity in an AREA, or
//! one wire id, from this shard's world AND the neighbours' lent
//! records, each wire id once, read into one game type — the merge a
//! game would otherwise write by hand (`docs/CROSS-SHARD.md` §4b).
//!
//! **Opt-in.** Nothing in the kit calls these: a game that never does
//! sees no change, and a game can still merge by hand ([`Seam::local`],
//! [`Seam::lent`], [`Seam::lent_iter`] and its own world queries).
//!
//! **One wire, one answer — the seam's precedence.** A wire id is found
//! where [`Seam::local`] / [`Seam::lent`] find it, so a hit and an
//! [`Seam::emit`] agree on where the entity lives:
//!
//! 1. **Local** — this shard's own entity, as a world query finds it: an
//!    entity handed on last tick is not (it is disabled for the game's
//!    hooks), nor is an entity the game despawned or disabled itself. An
//!    own entity's lent copy (it just migrated in) is never answered.
//! 2. **Departing** — an entity this shard handed on last tick: lent by
//!    its new owner, with the record it left with (the migration tick,
//!    `docs/CROSS-SHARD.md` §4d).
//! 3. **Lent** — a neighbour's border record. An entity handed between
//!    two neighbours can be lent by both for a tick: the lower lender
//!    index answers (the core's lookup order, where an effect is routed).
//!
//! Not found: a team import (W1 — a team's visibility, not gameplay
//! reach: it carries no typed record and no effect route), a record of a
//! quarantined view (the core hides it), an entity the game spawned in
//! this hook (it has no wire id until the kit stamps it after the
//! systems). A crystallization pin changes nothing: a held entity is
//! its holder's own. Reading does not count as a contact.
//!
//! **Cost.** A linear pass over this shard's owned-wire table (an O(1)
//! world lookup each) and the lent records (an O(lenders) lookup each,
//! for the precedence), then an in-place sort of the hits by wire id —
//! the answer's order is deterministic. No allocation beyond the
//! caller's buffer, which keeps its capacity across calls. The kit has
//! no spatial index of a shard's own entities that is current inside a
//! hook (the spatial composite's cells are last tick's wire values); a
//! game with its own index, or with a shard too large for a pass per
//! query, queries its index for the local half and [`Seam::find`]s or
//! [`Seam::lent_iter`]s the rest.

use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::{Entity, World};
use bevy_ecs::world::EntityRef;

use super::Seam;
use crate::space::Planar;

/// What a game reads of an entity it finds through the seam — the SAME
/// type whether the entity is its own or a neighbour's lent record (a
/// position, a side, whether it can be hit…). Implemented once per game
/// type; `V` is the game's wire value (the strip payload).
pub trait SeamView<V>: Sized {
    /// The view of this shard's own entity; `None` leaves it out (it is
    /// not what the query looks for — a component is missing).
    fn local(entity: EntityRef<'_>) -> Option<Self>;

    /// The view of a neighbour's lent record; `None` leaves it out.
    fn lent(record: &V) -> Option<Self>;
}

/// Where a found entity lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// This shard's own entity: write it directly.
    Local(Entity),
    /// A neighbour's: `lender` is its authority, where [`Seam::emit`]
    /// sends an effect on it (for an entity handed on last tick, its new
    /// owner).
    Lent { lender: usize },
}

/// One entity a seam query found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Found<T> {
    /// Its wire id.
    pub wire: u64,
    /// Where it lives.
    pub holder: Holder,
    /// What the game reads of it ([`SeamView`]).
    pub view: T,
}

impl<V> Seam<'_, '_, V> {
    /// The entity with wire id `wire`, local or lent (module docs, "one
    /// wire, one answer") — `None` when neither, or when the game's view
    /// leaves it out. A local entity's view is never replaced by a lent
    /// copy's.
    pub fn find<T: SeamView<V>>(&self, world: &World, wire: u64) -> Option<Found<T>> {
        if let Some(entity) = self.local(wire) {
            let view = visible(world, entity).and_then(T::local)?;
            return Some(Found {
                wire,
                holder: Holder::Local(entity),
                view,
            });
        }
        let lent = self.lent(wire)?;
        Some(Found {
            wire,
            holder: Holder::Lent {
                lender: lent.lender,
            },
            view: T::lent(lent.state)?,
        })
    }

    /// Every entity, local or lent, whose view `keep` accepts — each wire
    /// id once, as [`Self::find`] answers it — into `out` (cleared
    /// first), in ascending wire order. Cost: module docs.
    pub fn area<T: SeamView<V>>(
        &self,
        world: &World,
        mut keep: impl FnMut(&T) -> bool,
        out: &mut Vec<Found<T>>,
    ) {
        out.clear();
        for (&wire, &entity) in self.own {
            if self.departing.departing(wire, self.cross).is_some() {
                continue; // handed on: lent below, by its new owner
            }
            if let Some(view) = visible(world, entity).and_then(T::local)
                && keep(&view)
            {
                let holder = Holder::Local(entity);
                out.push(Found { wire, holder, view });
            }
        }
        for lent in self.lent_iter() {
            if let Some(view) = T::lent(lent.state)
                && keep(&view)
            {
                let holder = Holder::Lent {
                    lender: lent.lender,
                };
                out.push(Found {
                    wire: lent.wire,
                    holder,
                    view,
                });
            }
        }
        out.sort_unstable_by_key(|f| f.wire);
    }

    /// [`Self::area`] over a disc on the ground plane: every entity whose
    /// view's [`Planar`] projection lies within `radius` of `center` —
    /// the boundary included (distance ≤ `radius`).
    pub fn within<T>(&self, world: &World, center: [f32; 2], radius: f32, out: &mut Vec<Found<T>>)
    where
        T: SeamView<V> + Planar<Coord = f32>,
    {
        let r2 = radius * radius;
        self.area(
            world,
            |view: &T| {
                let [x, y] = view.planar();
                let (dx, dy) = (x - center[0], y - center[1]);
                dx * dx + dy * dy <= r2
            },
            out,
        );
    }
}

/// `entity` as a world query would see it: alive and not disabled.
fn visible(world: &World, entity: Entity) -> Option<EntityRef<'_>> {
    world
        .get_entity(entity)
        .ok()
        .filter(|e| !e.contains::<Disabled>())
}
