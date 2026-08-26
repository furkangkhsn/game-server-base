//! The per-cell change ledger: what entered, left or moved inside a
//! cell this tick, and the touch bookkeeping that decides whether a
//! group has anything to say at all.

use std::collections::{HashMap, HashSet};

use bevy_ecs::prelude::{Changed, Entity, World};

use crate::components::{Position, WireId};
use crate::common::*;

/// The per-tick CONTENT bookkeeping of a cell-encoded delta broadcaster —
/// the current buckets, the change lists, and the occupancy/member
/// baselines the classification rolls from. Own entities enter through
/// [`Self::dirty_pass`] (bevy's write path is the structural dirty mark);
/// any other content source (the sharded composite's borrowed border
/// strip) enters through the same four record primitives with
/// `member = false`, so both sources share one arithmetic.
#[derive(Default)]
pub(crate) struct CellBook {
    /// The current buckets: `cell → (wire id → (x, y))` — the content of
    /// every cell, maintained incrementally. Invariant: after a full tick
    /// body (dirty pass + every external source + roll), the buckets
    /// equal the visible world's current content.
    pub buckets: HashMap<Cell, HashMap<u64, (i32, i32)>>,
    /// The cells that were occupied at the last roll: the appearance/
    /// exit baseline. Frozen while content mutates, rolled only by
    /// [`Self::roll`] against the final bucket state (order-
    /// independence — a same-tick exit+entry cannot flip either flag).
    pub prev_occupied: HashSet<Cell>,
    /// Each bucketed entity's cell at the end of the last pass (written
    /// by the dirty pass, read by it, by removal parking, and by the
    /// O(1) `group_of` lookups of the broadcast phase).
    pub last_cell: HashMap<Entity, Cell>,
    /// The member count of each cell (empty entries removed): the
    /// birth arithmetic's "now" input. Borrowed records are never
    /// members — they carry no connection on this side.
    pub member_counts: HashMap<Cell, u32>,
    /// The cells touched this tick with their member-event counters
    /// (persistent map, cleared in place each tick): the roll iterates
    /// exactly this map — O(movers), never O(cells).
    pub touched: HashMap<Cell, TouchInfo>,
    /// Each touched cell's change list for this tick (persistent map,
    /// cleared in place each tick): the delta's source of truth.
    pub cell_changes: HashMap<Cell, CellChanges>,
    /// Removals parked by the CONTROL phase (leaves, migrations-out):
    /// a despawn is not a component write, so the dirty query cannot see
    /// it — the entity, its wire id and its last cell are parked here and
    /// applied by [`Self::apply_removals`]. A join+leave within one tick
    /// parks nothing — the entity never made it into `last_cell`, hence
    /// never into the buckets.
    pub pending_removals: Vec<(Entity, u64, Cell)>,
    /// The member entities (maintained by the room on join/leave/migrate):
    /// the dirty loop's O(1) membership test.
    pub members: HashSet<Entity>,
    /// Cells with members now but none at the last roll: their groups
    /// are fresh and must emit a FULL packet on their first tick.
    pub born_groups: HashSet<Cell>,
}

impl CellBook {
    /// Clear the per-tick state (persistent containers, in place).
    pub(crate) fn begin_tick(&mut self) {
        self.cell_changes.clear();
        self.touched.clear();
        self.born_groups.clear();
    }

    #[inline]
    fn touch(&mut self, c: Cell) {
        self.touched.entry(c).or_default();
    }

    #[inline]
    fn member_event(&mut self, c: Cell, in_: bool) {
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
    pub(crate) fn record_appearance(
        &mut self,
        wire: u64,
        x: i32,
        y: i32,
        cell: Cell,
        member: bool,
    ) {
        self.cell_changes
            .entry(cell)
            .or_default()
            .updates
            .push((wire, x, y));
        self.buckets.entry(cell).or_default().insert(wire, (x, y));
        self.touch(cell);
        if member {
            *self.member_counts.entry(cell).or_default() += 1;
            self.member_event(cell, true);
        }
    }

    /// Primitive: a record's wire content changed WITHIN `cell` (already
    /// known different — the quantization check belongs to the caller).
    pub(crate) fn record_update(&mut self, cell: Cell, wire: u64, x: i32, y: i32) {
        self.cell_changes
            .entry(cell)
            .or_default()
            .updates
            .push((wire, x, y));
        if let Some(b) = self.buckets.get_mut(&cell) {
            b.insert(wire, (x, y));
        }
        self.touch(cell);
    }

    /// Primitive: a record moved from `old` cell to `new` — an exit in
    /// the source, an upsert in the target (the packet passes fix the
    /// wire order: `removed` before `entities`).
    pub(crate) fn record_cross(
        &mut self,
        old: Cell,
        new: Cell,
        wire: u64,
        x: i32,
        y: i32,
        member: bool,
    ) {
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
            .push((wire, x, y));
        self.buckets.entry(new).or_default().insert(wire, (x, y));
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
    pub(crate) fn record_exit(&mut self, cell: Cell, wire: u64, member: bool) {
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
    /// `Position` write — by any writer, through any API — so the dirty
    /// mark lives in bevy's write path itself and no writer can forget
    /// it. Only changed entities are visited: per-tick work is
    /// proportional to movers, not to the entity count.
    ///
    /// Quantization: wire positions are i32 truncations of f32 motion —
    /// a same-cell move whose wire position did not change records
    /// nothing (the cell can still classify `Silent`), so the stream is
    /// content-identical to a diff-based design.
    pub(crate) fn dirty_pass(&mut self, world: &mut World, cell_size: f32) {
        let mut query =
            world.query_filtered::<(Entity, &WireId, &Position), Changed<Position>>();
        for (entity, wire_id, pos) in query.iter(world) {
            let wire = wire_id.get();
            let (x, y) = (pos.x as i32, pos.y as i32);
            let new_cell = cell_of(x, y, cell_size);
            let is_member = self.members.contains(&entity);
            match self.last_cell.get(&entity).copied() {
                None => {
                    // New this tick (a join, a migration-in, or a spawn
                    // between passes): an upsert in its cell.
                    self.record_appearance(wire, x, y, new_cell, is_member);
                    self.last_cell.insert(entity, new_cell);
                }
                Some(old) if old == new_cell => {
                    // Moved inside its cell — a record only when the wire
                    // content actually changed.
                    let changed = self
                        .buckets
                        .get(&old)
                        .and_then(|b| b.get(&wire))
                        .is_none_or(|&(px, py)| px != x || py != y);
                    if changed {
                        self.record_update(new_cell, wire, x, y);
                    }
                }
                Some(old) => {
                    self.record_cross(old, new_cell, wire, x, y, is_member);
                    self.last_cell.insert(entity, new_cell);
                }
            }
        }
    }

    /// Apply the removals parked during the CONTROL phase (despawns are
    /// invisible to the change query — module docs of the rooms).
    pub(crate) fn apply_removals(&mut self) {
        for (entity, wire, cell) in std::mem::take(&mut self.pending_removals) {
            self.last_cell.remove(&entity);
            // A parked removal is always a member's (connections own the
            // despawned entities); the join+leave-within-one-tick case
            // never parked a removal, so there is no count to undo for it.
            self.record_exit(cell, wire, /*member=*/ true);
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
