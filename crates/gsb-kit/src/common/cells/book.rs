//! The per-cell change ledger: what entered, left or moved inside a
//! cell this tick, and the touch bookkeeping that decides whether a
//! group has anything to say at all.

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;

use bevy_ecs::prelude::{Component, Entity, With, World};

use crate::codec::RecordCodec;
use crate::common::*;
use crate::identity::WireId;
use crate::space::CellSpace;

/// The per-tick CONTENT bookkeeping of a cell-encoded delta broadcaster —
/// the current buckets, the change lists, and the occupancy/member
/// baselines the classification rolls from. Own entities enter through
/// [`Self::dirty_pass`] (bevy's write path is the structural dirty mark);
/// any other content source (the sharded composite's borrowed border
/// strip) enters through the same four record primitives with
/// `member = false`, so both sources share one arithmetic.
///
/// Generic over the wire value `W` (the codec's `Wire` — the unit of the
/// change test) and the cell key `C` (the space's `Cell`).
pub(crate) struct CellBook<W, C> {
    /// The current buckets: `cell → (wire id → wire value)` — the content of
    /// every cell, maintained incrementally. Invariant: after a full tick
    /// body (dirty pass + every external source + roll), the buckets
    /// equal the visible world's current content.
    pub buckets: HashMap<C, HashMap<u64, W>>,
    /// The cells that were occupied at the last roll: the appearance/
    /// exit baseline. Frozen while content mutates, rolled only by
    /// [`Self::roll`] against the final bucket state (order-
    /// independence — a same-tick exit+entry cannot flip either flag).
    pub prev_occupied: HashSet<C>,
    /// Each bucketed entity's wire id and cell at the end of the last
    /// pass (written by the dirty pass, read by it, by the removal
    /// passes, and by the O(1) `group_of` lookups of the broadcast
    /// phase). The wire id is kept here because a despawned entity's
    /// components are gone: this is what [`Self::sweep_removed`] learns
    /// a despawned entity's record from.
    pub last_cell: HashMap<Entity, (u64, C)>,
    /// The member count of each cell (empty entries removed): the
    /// birth arithmetic's "now" input. Borrowed records are never
    /// members — they carry no connection on this side.
    pub member_counts: HashMap<C, u32>,
    /// The cells touched this tick with their member-event counters
    /// (persistent map, cleared in place each tick): the roll iterates
    /// exactly this map — O(movers), never O(cells).
    pub touched: HashMap<C, TouchInfo>,
    /// Each touched cell's change list for this tick (persistent map,
    /// cleared in place each tick): the delta's source of truth.
    pub cell_changes: HashMap<C, CellChanges<W>>,
    /// Removals parked by the CONTROL phase (leaves, migrations-out):
    /// a despawn is not a component write, so the dirty query cannot see
    /// it — the entity and whether it counted as a member are parked
    /// here and applied by [`Self::apply_removals`] against `last_cell`.
    /// A join+leave within one tick parks nothing — the entity never
    /// made it into `last_cell`, hence never into the buckets. (Despawns
    /// nobody parks — game code despawning an NPC — are found by
    /// [`Self::sweep_removed`].)
    pub pending_removals: Vec<(Entity, bool)>,
    /// The member entities (maintained by the room on join/leave/migrate):
    /// the dirty loop's O(1) membership test.
    pub members: HashSet<Entity>,
    /// Cells with members now but none at the last roll: their groups
    /// are fresh and must emit a FULL packet on their first tick.
    pub born_groups: HashSet<C>,
}

// Not derived: a derive would demand `W: Default` and `C: Default`.
impl<W, C> Default for CellBook<W, C> {
    fn default() -> Self {
        Self {
            buckets: HashMap::new(),
            prev_occupied: HashSet::new(),
            last_cell: HashMap::new(),
            member_counts: HashMap::new(),
            touched: HashMap::new(),
            cell_changes: HashMap::new(),
            pending_removals: Vec::new(),
            members: HashSet::new(),
            born_groups: HashSet::new(),
        }
    }
}

impl<W: Clone + Eq, C: Copy + Eq + Hash + Debug> CellBook<W, C> {
    /// Clear the per-tick state (persistent containers, in place).
    pub(crate) fn begin_tick(&mut self) {
        self.cell_changes.clear();
        self.touched.clear();
        self.born_groups.clear();
    }

    #[inline]
    fn touch(&mut self, c: C) {
        self.touched.entry(c).or_default();
    }

    #[inline]
    fn member_event(&mut self, c: C, in_: bool) {
        let t = self.touched.entry(c).or_default();
        if in_ {
            t.member_in += 1;
        } else {
            t.member_out += 1;
        }
    }

    /// Primitive: a record NEWLY occupies `cell` (a join spawn, a fresh
    /// migration-in, a borrowed record entering the view). An upsert in
    /// the cell's change list.
    pub(crate) fn record_appearance(&mut self, wire: u64, value: W, cell: C, member: bool) {
        self.cell_changes
            .entry(cell)
            .or_default()
            .updates
            .push((wire, value.clone()));
        self.buckets.entry(cell).or_default().insert(wire, value);
        self.touch(cell);
        if member {
            *self.member_counts.entry(cell).or_default() += 1;
            self.member_event(cell, true);
        }
    }

    /// Primitive: a record's wire content changed WITHIN `cell` (already
    /// known different — the quantization check belongs to the caller).
    pub(crate) fn record_update(&mut self, cell: C, wire: u64, value: W) {
        self.cell_changes
            .entry(cell)
            .or_default()
            .updates
            .push((wire, value.clone()));
        if let Some(b) = self.buckets.get_mut(&cell) {
            b.insert(wire, value);
        }
        self.touch(cell);
    }

    /// Primitive: a record moved from `old` cell to `new` — an exit in
    /// the source, an upsert in the target (the packet passes fix the
    /// wire order: `removed` before `entities`).
    pub(crate) fn record_cross(&mut self, old: C, new: C, wire: u64, value: W, member: bool) {
        self.cell_changes.entry(old).or_default().exits.push(wire);
        if let Some(b) = self.buckets.get_mut(&old) {
            b.remove(&wire);
            if b.is_empty() {
                self.buckets.remove(&old);
            }
        }
        self.touch(old);
        self.cell_changes
            .entry(new)
            .or_default()
            .updates
            .push((wire, value.clone()));
        self.buckets.entry(new).or_default().insert(wire, value);
        self.touch(new);
        if member {
            if let Some(n) = self.member_counts.get_mut(&old) {
                *n -= 1;
                if *n == 0 {
                    self.member_counts.remove(&old);
                }
            }
            *self.member_counts.entry(new).or_default() += 1;
            self.member_event(old, false);
            self.member_event(new, true);
        }
    }

    /// Primitive: a record LEFT `cell` without a tracked position write
    /// (a parked despawn removal, a borrowed record exiting the view).
    pub(crate) fn record_exit(&mut self, cell: C, wire: u64, member: bool) {
        self.cell_changes.entry(cell).or_default().exits.push(wire);
        if let Some(b) = self.buckets.get_mut(&cell) {
            b.remove(&wire);
            if b.is_empty() {
                self.buckets.remove(&cell);
            }
        }
        self.touch(cell);
        if member {
            if let Some(n) = self.member_counts.get_mut(&cell) {
                *n -= 1;
                if *n == 0 {
                    self.member_counts.remove(&cell);
                }
            }
            self.member_event(cell, false);
        }
    }

    /// The own-entity dirty pass: bevy's change detection flags every
    /// write the codec's [`RecordCodec::Dirty`] filter names (the demo:
    /// `Changed<Position>`) — by any writer, through any API — so the
    /// dirty mark lives in bevy's write path itself and no writer can
    /// forget it. Only changed entities are visited: per-tick work is
    /// proportional to movers, not to the entity count.
    ///
    /// Quantization: the change test is on the WIRE value (the demo's
    /// i32 truncation of f32 motion) — a same-cell move whose wire value
    /// did not change records nothing (the cell can still classify
    /// `Silent`), so the stream is content-identical to a diff-based
    /// design.
    pub(crate) fn dirty_pass<R, S>(&mut self, world: &mut World, codec: &R, space: &S)
    where
        R: RecordCodec<Wire = W>,
        S: CellSpace<W, Cell = C>,
    {
        let mut query =
            world.query_filtered::<(Entity, &WireId, R::Query), (R::Dirty, With<R::Marker>)>();
        for (entity, wire_id, item) in query.iter(world) {
            let wire = wire_id.get();
            let value = codec.wire(item);
            let new_cell = space.cell_of(&value);
            let is_member = self.members.contains(&entity);
            match self.last_cell.get(&entity).map(|&(_, c)| c) {
                None => {
                    // New this tick (a join, a migration-in, or a spawn
                    // between passes): an upsert in its cell.
                    self.record_appearance(wire, value, new_cell, is_member);
                    self.last_cell.insert(entity, (wire, new_cell));
                }
                Some(old) if old == new_cell => {
                    // Moved inside its cell — a record only when the wire
                    // content actually changed.
                    let changed = self
                        .buckets
                        .get(&old)
                        .and_then(|b| b.get(&wire))
                        .is_none_or(|prev| *prev != value);
                    if changed {
                        self.record_update(new_cell, wire, value);
                    }
                }
                Some(old) => {
                    self.record_cross(old, new_cell, wire, value, is_member);
                    self.last_cell.insert(entity, (wire, new_cell));
                }
            }
        }
    }

    /// The cell `entity`'s records landed in at the end of the last pass.
    #[inline]
    pub(crate) fn cell_of_entity(&self, entity: &Entity) -> Option<C> {
        self.last_cell.get(entity).map(|&(_, c)| c)
    }

    /// Apply the removals parked during the CONTROL phase (despawns are
    /// invisible to the change query — module docs of the rooms).
    pub(crate) fn apply_removals(&mut self) {
        for (entity, member) in std::mem::take(&mut self.pending_removals) {
            if let Some((wire, cell)) = self.last_cell.remove(&entity) {
                self.record_exit(cell, wire, member);
            }
        }
    }

    /// The despawns (and broadcast-marker removals) nobody parked — game
    /// code despawning an NPC, a bullet expiring — read from the world's
    /// removed-component buffers (§8.2: without this they stayed in the
    /// buckets as ghosts every later packet re-carried). Runs after
    /// [`Self::apply_removals`] (a parked removal has left `last_cell`,
    /// so it is not counted twice) and BEFORE the tick's change-window
    /// close, which empties the buffers (§4.4 — the kit is the single
    /// caller of `World::clear_trackers`, once per tick, so every
    /// despawn since the previous close is in the buffer exactly once).
    ///
    /// An entity that lost the marker `M` but is still alive leaves the
    /// view the same way; one whose marker came back within the tick
    /// (still carrying the same wire id) was placed by the dirty pass and
    /// stays.
    pub(crate) fn sweep_removed<M: Component>(&mut self, world: &World) {
        for entity in world.removed::<WireId>().chain(world.removed::<M>()) {
            let Some(&(wire, cell)) = self.last_cell.get(&entity) else {
                continue; // never bucketed, or already removed
            };
            let alive = world.get_entity(entity).ok();
            let still_placed = alive.is_some_and(|e| {
                e.contains::<M>() && e.get::<WireId>().is_some_and(|w| w.get() == wire)
            });
            if still_placed {
                continue;
            }
            self.last_cell.remove(&entity);
            let member = if alive.is_some() {
                self.members.contains(&entity)
            } else {
                self.members.remove(&entity)
            };
            self.record_exit(cell, wire, member);
        }
    }

    /// The per-cell flags, the group births, and the occupancy roll —
    /// all order-independent: `prev_occupied` was frozen while content
    /// mutated and is rolled here against the FINAL bucket state, and
    /// the member arithmetic reconstructs the before-tick count from the
    /// net events (`now − in + out`). Runs after EVERY content source of
    /// the tick has landed — on the sharded composite that includes the
    /// borrowed-strip integration, which is why the roll cannot simply
    /// sit at the end of `update` there.
    pub(crate) fn roll(&mut self) {
        for (c, t) in self.touched.iter() {
            let occupied_now = self.buckets.contains_key(c);
            let occupied_prev = self.prev_occupied.contains(c);
            let ch = self
                .cell_changes
                .get_mut(c)
                .expect("a touched cell has a change entry");
            ch.appeared = occupied_now && !occupied_prev;
            ch.exited = occupied_prev && !occupied_now;
            let now = self.member_counts.get(c).copied().unwrap_or(0);
            let before = now.wrapping_sub(t.member_in).wrapping_add(t.member_out);
            debug_assert!(
                before.saturating_add(t.member_in) >= t.member_out,
                "member count went negative for {c:?}"
            );
            if before == 0 && now > 0 {
                self.born_groups.insert(*c);
            }
            if occupied_now {
                self.prev_occupied.insert(*c);
            } else {
                self.prev_occupied.remove(c);
            }
        }
    }
}
