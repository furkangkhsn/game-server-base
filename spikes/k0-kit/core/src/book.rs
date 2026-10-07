//! The per-cell change ledger — `gsb-kit`'s `common/cells/book.rs` with
//! the two ECS reads cut out:
//!
//! - `dirty_pass` (a bevy `Changed<T>` query) becomes [`CellBook::observe`],
//!   the loop BODY: a binding iterates its own change source and calls it
//!   once per possibly-changed record;
//! - `sweep_removed` (bevy's removed-component buffers) becomes
//!   [`CellBook::forget`], called once per record a binding found gone.
//!
//! Every other line — the four primitives, the parked removals, the
//! order-independent roll — is the kit's, with `Entity` keys replaced by
//! [`Owner`]. (The send-rate deferral and the lit room's entity index
//! are left out of the spike: neither touches the ECS.)

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;

use crate::owner::Owner;
use crate::space::CellSpace;

/// One cell's changes this tick (the delta's source of truth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellChanges<W> {
    /// Records whose wire content changed, or that newly occupy the cell.
    pub updates: Vec<(u64, W)>,
    /// Wire ids that left the cell.
    pub exits: Vec<u64>,
    /// The cell was empty at the last roll.
    pub appeared: bool,
    /// The cell is empty now (and was not then).
    pub exited: bool,
}

impl<W> Default for CellChanges<W> {
    fn default() -> Self {
        Self {
            updates: Vec::new(),
            exits: Vec::new(),
            appeared: false,
            exited: false,
        }
    }
}

/// The per-tick member-event counters of one touched cell.
#[derive(Debug, Default)]
pub struct TouchInfo {
    /// Members that entered the cell this tick.
    pub member_in: u32,
    /// Members that left it.
    pub member_out: u32,
}

/// The cell-delta bookkeeping, keyed by [`Owner`] instead of `Entity`.
pub struct CellBook<W, C> {
    /// `cell → (wire id → wire value)`: the current content.
    pub buckets: HashMap<C, HashMap<u64, W>>,
    /// The cells occupied at the last roll.
    pub prev_occupied: HashSet<C>,
    /// Each bucketed owner's wire id and cell at the end of the last pass.
    pub last_cell: HashMap<Owner, (u64, C)>,
    /// The member count of each cell.
    pub member_counts: HashMap<C, u32>,
    /// The cells touched this tick.
    pub touched: HashMap<C, TouchInfo>,
    /// Each touched cell's change list.
    pub cell_changes: HashMap<C, CellChanges<W>>,
    /// Removals parked by the CONTROL phase (leaves): `(owner, member)`.
    pub pending_removals: Vec<(Owner, bool)>,
    /// The member owners.
    pub members: HashSet<Owner>,
    /// Cells whose groups are fresh this tick.
    pub born_groups: HashSet<C>,
}

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

impl<W: Clone + Eq, C: Copy + Eq + Hash + Ord + Debug> CellBook<W, C> {
    /// Clear the per-tick state (persistent containers, in place).
    pub fn begin_tick(&mut self) {
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

    fn record_appearance(&mut self, wire: u64, value: W, cell: C, member: bool) {
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

    fn record_update(&mut self, cell: C, wire: u64, value: W) {
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

    fn record_cross(&mut self, old: C, new: C, wire: u64, value: W, member: bool) {
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

    fn record_exit(&mut self, cell: C, wire: u64, member: bool) {
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

    /// One possibly-changed record (the body of `gsb-kit`'s dirty pass,
    /// verbatim): new → an appearance; same cell → an update when the
    /// WIRE value differs (the quantization no-op); another cell → a
    /// crossing. A binding calls this for every record its change source
    /// flags; a binding without change detection may call it for EVERY
    /// record (the same-cell compare makes an unchanged record a no-op —
    /// at the price of a hash lookup per record).
    #[inline]
    pub fn observe<S: CellSpace<W, Cell = C>>(
        &mut self,
        space: &S,
        owner: Owner,
        wire: u64,
        value: W,
    ) {
        let new_cell = space.cell_of(&value);
        let is_member = self.members.contains(&owner);
        match self.last_cell.get(&owner).map(|&(_, c)| c) {
            None => {
                self.record_appearance(wire, value, new_cell, is_member);
                self.last_cell.insert(owner, (wire, new_cell));
            }
            Some(old) if old == new_cell => {
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
                self.last_cell.insert(owner, (wire, new_cell));
            }
        }
    }

    /// A member joined (the room's join path).
    pub fn add_member(&mut self, owner: Owner) {
        self.members.insert(owner);
    }

    /// A member left (the room's `on_leave`): parked, applied by
    /// [`Self::apply_removals`] — a despawn is not a write.
    pub fn park_leave(&mut self, owner: Owner) {
        self.members.remove(&owner);
        if self.last_cell.contains_key(&owner) {
            self.pending_removals.push((owner, true));
        }
    }

    /// Apply the parked removals.
    pub fn apply_removals(&mut self) {
        for (owner, member) in std::mem::take(&mut self.pending_removals) {
            if let Some((wire, cell)) = self.last_cell.remove(&owner) {
                self.record_exit(cell, wire, member);
            }
        }
    }

    /// A record a binding found gone without a parked removal (`gsb-kit`'s
    /// `sweep_removed` body): `alive` = the owner still exists but left
    /// the broadcast set.
    pub fn forget(&mut self, owner: Owner, alive: bool) {
        let Some((wire, cell)) = self.last_cell.remove(&owner) else {
            return;
        };
        let member = if alive {
            self.members.contains(&owner)
        } else {
            self.members.remove(&owner)
        };
        self.record_exit(cell, wire, member);
    }

    /// The order-independent roll (verbatim).
    pub fn roll(&mut self) {
        for (c, t) in &self.touched {
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

    /// NEW in the agnostic core: make the change lists independent of the
    /// order the binding observed records in (an ECS's archetype order).
    /// Each touched cell's updates and exits are sorted by wire id —
    /// O(m log m) over the tick's changes only. This replaces what
    /// `gsb-kit`'s `Cached` buys with the archetype-generation rebuild
    /// (A12's byte identity is a bevy-order property; this one holds for
    /// every binding).
    pub fn canonicalize(&mut self) {
        for ch in self.cell_changes.values_mut() {
            if ch.updates.len() > 1 {
                ch.updates.sort_unstable_by_key(|&(w, _)| w);
            }
            if ch.exits.len() > 1 {
                ch.exits.sort_unstable();
            }
        }
    }

    /// The cell `owner`'s record landed in at the last pass.
    #[must_use]
    pub fn cell_of_owner(&self, owner: Owner) -> Option<C> {
        self.last_cell.get(&owner).map(|&(_, c)| c)
    }
}
