//! TODAY's kit path, copied from `crates/gsb-kit/src/common/cells/book.rs`
//! (dirty pass + parked removals + `sweep_removed` + roll) and
//! `aoi/logic.rs` (`update`'s order), with the send-rate deferral and the
//! entity index left out (as in the core) — the benchmark's baseline.
//! `Entity`-keyed, the bevy query INSIDE the book. Only the type names
//! differ (`KitWire` for the kit's `WireId` component).

use std::collections::{HashMap, HashSet};

use bevy_ecs::prelude::{Changed, Entity, With, World};
use k0_kit_bevy_ecs::{Cached, KitWire};
use k0_kit_core::{Cell, CellChanges, CellSpace, Grid2, Minter, TouchInfo};

use crate::{CELL_SIZE, Canon, Driver, Op, P2, Position, canon};

type DirtyPass =
    Cached<(Entity, &'static KitWire, &'static Position), (Changed<Position>, With<Position>)>;

/// Today's `CellBook<Wire, Cell>` (fields as in the kit).
#[derive(Default)]
struct Book {
    buckets: HashMap<Cell, HashMap<u64, P2>>,
    prev_occupied: HashSet<Cell>,
    last_cell: HashMap<Entity, (u64, Cell)>,
    member_counts: HashMap<Cell, u32>,
    touched: HashMap<Cell, TouchInfo>,
    cell_changes: HashMap<Cell, CellChanges<P2>>,
    pending_removals: Vec<(Entity, bool)>,
    members: HashSet<Entity>,
    born_groups: HashSet<Cell>,
}

impl Book {
    fn begin_tick(&mut self) {
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
    fn record_appearance(&mut self, wire: u64, value: P2, cell: Cell, member: bool) {
        self.cell_changes
            .entry(cell)
            .or_default()
            .updates
            .push((wire, value));
        self.buckets.entry(cell).or_default().insert(wire, value);
        self.touch(cell);
        if member {
            *self.member_counts.entry(cell).or_default() += 1;
            self.member_event(cell, true);
        }
    }
    fn record_update(&mut self, cell: Cell, wire: u64, value: P2) {
        self.cell_changes
            .entry(cell)
            .or_default()
            .updates
            .push((wire, value));
        if let Some(b) = self.buckets.get_mut(&cell) {
            b.insert(wire, value);
        }
        self.touch(cell);
    }
    fn record_cross(&mut self, old: Cell, new: Cell, wire: u64, value: P2, member: bool) {
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
            .push((wire, value));
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
    fn record_exit(&mut self, cell: Cell, wire: u64, member: bool) {
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

    /// `CellBook::dirty_pass`, as in the kit.
    fn dirty_pass(&mut self, query: &mut DirtyPass, world: &mut World, space: &Grid2) {
        for (entity, wire_id, item) in query.state(world).iter(world) {
            let wire = wire_id.0.get();
            let value = k0_kit_core::ToWire::wire(item);
            let new_cell = space.cell_of(&value);
            let is_member = self.members.contains(&entity);
            match self.last_cell.get(&entity).map(|&(_, c)| c) {
                None => {
                    self.record_appearance(wire, value, new_cell, is_member);
                    self.last_cell.insert(entity, (wire, new_cell));
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
                    self.last_cell.insert(entity, (wire, new_cell));
                }
            }
        }
    }

    fn apply_removals(&mut self) {
        for (entity, member) in std::mem::take(&mut self.pending_removals) {
            if let Some((wire, cell)) = self.last_cell.remove(&entity) {
                self.record_exit(cell, wire, member);
            }
        }
    }

    /// `CellBook::sweep_removed::<Position>`, as in the kit.
    fn sweep_removed(&mut self, world: &World) {
        for entity in world
            .removed::<KitWire>()
            .chain(world.removed::<Position>())
        {
            let Some(&(wire, cell)) = self.last_cell.get(&entity) else {
                continue;
            };
            let alive = world.get_entity(entity).ok();
            let still_placed = alive.is_some_and(|e| {
                e.contains::<Position>() && e.get::<KitWire>().is_some_and(|w| w.0.get() == wire)
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

    fn roll(&mut self) {
        for (c, t) in &self.touched {
            let occupied_now = self.buckets.contains_key(c);
            let occupied_prev = self.prev_occupied.contains(c);
            let ch = self.cell_changes.get_mut(c).expect("touched");
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
}

/// Today's AOI path on a bevy world.
pub struct NativeBevy {
    world: World,
    book: Book,
    dirty: DirtyPass,
    space: Grid2,
    minter: Minter,
    slots: Vec<Option<(Entity, bool)>>,
}

impl NativeBevy {
    /// A fresh driver with `slots` logical slots.
    #[must_use]
    pub fn new(slots: usize) -> Self {
        Self {
            world: World::new(),
            book: Book::default(),
            dirty: Cached::default(),
            space: Grid2::new(CELL_SIZE),
            minter: Minter::default(),
            slots: vec![None; slots],
        }
    }
}

impl Driver for NativeBevy {
    fn apply(&mut self, ops: &[Op]) {
        for op in ops {
            match *op {
                Op::Spawn { slot, pos, member } => {
                    // `common::join_with`: the game spawns, the kit stamps.
                    let e = self.world.spawn((pos, KitWire(self.minter.mint()))).id();
                    if member {
                        self.book.members.insert(e);
                    }
                    self.slots[slot] = Some((e, member));
                }
                Op::Move { slot, pos } => {
                    if let Some((e, _)) = self.slots[slot]
                        && let Some(mut p) = self.world.get_mut::<Position>(e)
                    {
                        *p = pos;
                    }
                }
                Op::Despawn { slot } => {
                    if let Some((e, member)) = self.slots[slot].take() {
                        if member {
                            // `AoiRoom::on_leave`.
                            self.book.members.remove(&e);
                            if self.book.last_cell.contains_key(&e) {
                                self.book.pending_removals.push((e, true));
                            }
                        }
                        self.world.despawn(e);
                    }
                }
            }
        }
    }

    /// `AoiRoom::update` after the systems (orphan stamp: none here).
    #[inline]
    fn update(&mut self) {
        self.book.begin_tick();
        self.book
            .dirty_pass(&mut self.dirty, &mut self.world, &self.space);
        self.book.apply_removals();
        self.book.sweep_removed(&self.world);
        self.book.roll();
        self.world.clear_trackers();
    }

    fn canon(&self) -> Canon {
        canon(
            self.book.buckets.iter(),
            self.book.cell_changes.iter(),
            self.book.born_groups.iter(),
            self.book.member_counts.iter(),
        )
    }

    fn packets(&self, _tick: u64) -> Option<Vec<(Cell, Vec<u8>)>> {
        None
    }
}
