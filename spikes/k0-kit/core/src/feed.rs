//! [`CellFeed`] — the ONE seam between the cell-delta core and an ECS:
//! what changed, what is gone, and the close of the change window. A
//! binding crate implements it for its world; the core drives it from
//! [`run_tick`] (the body of `AoiRoom::update` after the game's systems).

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

use crate::book::CellBook;
use crate::owner::Owner;
use crate::space::CellSpace;

/// A record that left the broadcast set without a parked removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gone {
    /// Its owner.
    pub owner: Owner,
    /// The owner still exists (it lost the marker / its wire id).
    pub alive: bool,
}

/// What a binding feeds the cell book each tick. `W` is the wire value.
pub trait CellFeed<W> {
    /// The binding's world type.
    type World;

    /// Every record whose value MAY have changed since the last
    /// [`Self::close`] — `(owner, wire id, wire value)`. Over-reporting
    /// is harmless (the book's same-cell compare drops it); missing a
    /// write is the one bug, so a binding must flag every writer
    /// structurally (bevy: `Changed<T>`; hecs: `Tracked<T>`'s
    /// `DerefMut`).
    fn changed(&mut self, world: &mut Self::World, sink: impl FnMut(Owner, u64, W));

    /// Every bucketed record that left the broadcast set since the last
    /// close without a parked removal (a game despawn, a marker removed).
    /// `known` is the book's `owner → (wire id, cell)` table — what a
    /// binding without removal events checks liveness against.
    fn gone<C>(
        &mut self,
        world: &Self::World,
        known: &HashMap<Owner, (u64, C)>,
        sink: impl FnMut(Gone),
    );

    /// Close the tick's change window (bevy: `World::clear_trackers`;
    /// hecs `Tracked<T>`: the flags were cleared while read — a no-op).
    fn close(&mut self, world: &mut Self::World);
}

/// One tick of the cell path after the game's systems, ECS-free: the
/// order of `AoiRoom::update` (dirty pass, parked removals, the sweep,
/// the roll, the window close) — plus [`CellBook::canonicalize`].
pub fn run_tick<W, S, F>(
    book: &mut CellBook<W, S::Cell>,
    space: &S,
    feed: &mut F,
    world: &mut F::World,
    gone: &mut Vec<Gone>,
) where
    W: Clone + Eq,
    S: CellSpace<W>,
    S::Cell: Copy + Eq + Hash + Ord + Debug,
    F: CellFeed<W>,
{
    book.begin_tick();
    feed.changed(world, |owner, wire, value| {
        book.observe(space, owner, wire, value);
    });
    book.apply_removals();
    gone.clear();
    feed.gone(world, &book.last_cell, |g| gone.push(g));
    for g in gone.iter() {
        book.forget(g.owner, g.alive);
    }
    book.canonicalize();
    book.roll();
    feed.close(world);
}
