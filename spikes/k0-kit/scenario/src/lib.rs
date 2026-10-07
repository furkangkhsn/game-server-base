//! k0 spike rig: one scripted world (spawns, moves, despawns) played
//! through four drivers —
//!
//! - [`native::NativeBevy`]: today's `gsb-kit` AOI path, a verbatim copy
//!   (bevy `Entity` keys, the `Changed<T>` dirty pass inside the book);
//! - [`Agnostic`] over [`BevyCellFeed`]: the ECS-free core + the bevy_ecs
//!   binding;
//! - [`Agnostic`] over [`HecsTrackedFeed`]: the core + the hecs binding
//!   (`Tracked<T>` flags);
//! - [`Agnostic`] over [`HecsDiffFeed`]: the core + hecs with no change
//!   source (every record, every tick).
//!
//! The tests (`tests/equal.rs`) check every driver ends every tick in the
//! same canonical state, and the agnostic ones emit the same packet
//! bytes; `src/bin/bench.rs` times them.

pub mod native;

use std::collections::{BTreeMap, BTreeSet};

use bevy_ecs::component::Component;
use k0_kit_bevy_ecs::{BevyCellFeed, KitWire};
use k0_kit_core::{
    Cell, CellBook, CellFeed, Gone, Grid2, Minter, Owner, Planar, ToWire, WireId, WireValue,
    assemble_group_packet, run_tick,
};
use k0_kit_hecs::{HecsDiffFeed, HecsTrackedFeed, Tracked};

/// The scenario's AOI cell edge (wire units).
pub const CELL_SIZE: f32 = 64.0;

/// A simulation position (f32), the record component.
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub struct Position {
    /// x
    pub x: f32,
    /// y
    pub y: f32,
}

/// The wire value: the truncated position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct P2(pub i32, pub i32);

impl Planar for P2 {
    #[inline]
    fn planar(&self) -> [i32; 2] {
        [self.0, self.1]
    }
}

impl WireValue for P2 {
    fn encode(&self, out: &mut Vec<u8>) {
        zigzag(self.0, out);
        zigzag(self.1, out);
    }
}

fn zigzag(v: i32, out: &mut Vec<u8>) {
    let mut z = ((v << 1) ^ (v >> 31)) as u32;
    while z >= 0x80 {
        out.push((z as u8) | 0x80);
        z >>= 7;
    }
    out.push(z as u8);
}

impl ToWire for Position {
    type Wire = P2;
    #[inline]
    fn wire(&self) -> P2 {
        P2(self.x as i32, self.y as i32)
    }
}

// ── The script ─────────────────────────────────────────────────────────

/// One scripted game action on a logical slot.
#[derive(Debug, Clone, Copy)]
pub enum Op {
    /// Spawn into `slot` (a member = a player: its leave is parked).
    Spawn {
        /// slot
        slot: usize,
        /// position
        pos: Position,
        /// a player
        member: bool,
    },
    /// Write the slot's position (may be a sub-quantum move).
    Move {
        /// slot
        slot: usize,
        /// new position
        pos: Position,
    },
    /// Despawn the slot (a member: the room's leave; else a game
    /// despawn, found by the binding's sweep).
    Despawn {
        /// slot
        slot: usize,
    },
}

/// xorshift64* — deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// A precomputed script: the initial population, then one op list per
/// tick.
pub struct Script {
    /// The initial spawns.
    pub init: Vec<Op>,
    /// The per-tick ops.
    pub ticks: Vec<Vec<Op>>,
    /// Slot capacity.
    pub slots: usize,
}

/// `n` records (10% players) on a square map of ~10 records per 64×64
/// cell; per tick `move_pct`% of the records move (1 in 8 a sub-quantum
/// move), `churn_per_mille`‰ of the NPCs despawn and as many spawn, and
/// one player leaves and one joins.
#[must_use]
pub fn script(
    n: usize,
    ticks: usize,
    move_pct: usize,
    churn_per_mille: usize,
    seed: u64,
) -> Script {
    let mut rng = Rng(seed | 1);
    let side = ((n as f32 / 10.0).sqrt() * CELL_SIZE).max(CELL_SIZE * 3.0);
    let mut pos: Vec<Option<(Position, bool)>> = Vec::new();
    let mut init = Vec::new();
    let place = |rng: &mut Rng| Position {
        x: rng.unit() * side,
        y: rng.unit() * side,
    };
    for slot in 0..n {
        let p = place(&mut rng);
        let member = slot % 10 == 0;
        pos.push(Some((p, member)));
        init.push(Op::Spawn {
            slot,
            pos: p,
            member,
        });
    }
    let mut out = Vec::with_capacity(ticks);
    for _ in 0..ticks {
        let mut ops = Vec::new();
        let alive: Vec<usize> = (0..pos.len()).filter(|&s| pos[s].is_some()).collect();
        for _ in 0..(alive.len() * move_pct / 100) {
            let s = alive[rng.below(alive.len())];
            let Some((p, m)) = pos[s] else { continue };
            let step = if rng.below(8) == 0 { 0.2 } else { 6.0 };
            let np = Position {
                x: (p.x + (rng.unit() - 0.5) * 2.0 * step).clamp(0.0, side),
                y: (p.y + (rng.unit() - 0.5) * 2.0 * step).clamp(0.0, side),
            };
            pos[s] = Some((np, m));
            ops.push(Op::Move { slot: s, pos: np });
        }
        let churn = (alive.len() * churn_per_mille / 1000).max(1);
        let mut killed = 0;
        let mut left = false;
        for _ in 0..churn * 4 {
            let s = alive[rng.below(alive.len())];
            match pos[s] {
                Some((_, false)) if killed < churn => {
                    pos[s] = None;
                    killed += 1;
                    ops.push(Op::Despawn { slot: s });
                }
                Some((_, true)) if !left => {
                    pos[s] = None;
                    left = true;
                    ops.push(Op::Despawn { slot: s });
                }
                _ => {}
            }
        }
        for i in 0..killed + usize::from(left) {
            let slot = pos.len();
            let p = place(&mut rng);
            let member = left && i == 0;
            pos.push(Some((p, member)));
            ops.push(Op::Spawn {
                slot,
                pos: p,
                member,
            });
        }
        out.push(ops);
    }
    Script {
        init,
        ticks: out,
        slots: pos.len(),
    }
}

// ── The canonical state every driver must agree on ─────────────────────

/// One cell's change list, sorted.
pub type CanonChanges = (Vec<(u64, P2)>, Vec<u64>, bool, bool);

/// A driver's book state, in a canonical (sorted) form.
#[derive(Debug, PartialEq, Eq)]
pub struct Canon {
    /// cell → wire id → value
    pub buckets: BTreeMap<Cell, BTreeMap<u64, P2>>,
    /// cell → (updates, exits, appeared, exited)
    pub changes: BTreeMap<Cell, CanonChanges>,
    /// born groups
    pub born: BTreeSet<Cell>,
    /// member counts
    pub members: BTreeMap<Cell, u32>,
}

/// Canonicalize a book's maps (any key type).
pub fn canon<'a>(
    buckets: impl Iterator<Item = (&'a Cell, &'a std::collections::HashMap<u64, P2>)>,
    changes: impl Iterator<Item = (&'a Cell, &'a k0_kit_core::CellChanges<P2>)>,
    born: impl Iterator<Item = &'a Cell>,
    members: impl Iterator<Item = (&'a Cell, &'a u32)>,
) -> Canon {
    Canon {
        buckets: buckets
            .map(|(c, b)| (*c, b.iter().map(|(w, v)| (*w, *v)).collect()))
            .collect(),
        changes: changes
            .map(|(c, ch)| {
                let mut u = ch.updates.clone();
                u.sort_unstable();
                let mut e = ch.exits.clone();
                e.sort_unstable();
                (*c, (u, e, ch.appeared, ch.exited))
            })
            .collect(),
        born: born.copied().collect(),
        members: members.map(|(c, n)| (*c, *n)).collect(),
    }
}

// ── The drivers ────────────────────────────────────────────────────────

/// A world + kit pair the scenario drives.
pub trait Driver {
    /// The game phase: apply the tick's ops (writes, spawns, despawns).
    fn apply(&mut self, ops: &[Op]);
    /// The kit phase (the timed part): `AoiRoom::update` after the
    /// game's systems.
    fn update(&mut self);
    /// The canonical state after the last update.
    fn canon(&self) -> Canon;
    /// Every member cell's packet for `tick` (agnostic drivers only).
    fn packets(&self, tick: u64) -> Option<Vec<(Cell, Vec<u8>)>>;
}

/// The game side of an agnostic driver: how a scripted op lands in this
/// ECS (what a game crate would write — not kit code).
pub trait GameSide: Default {
    /// The world.
    type World;
    /// The kit's feed for this world.
    type Feed: CellFeed<P2, World = Self::World> + Default;
    /// A fresh world.
    fn world() -> Self::World;
    /// Spawn a record with its wire id.
    fn spawn(w: &mut Self::World, pos: Position, wire: WireId) -> Owner;
    /// Write a record's position.
    fn write(w: &mut Self::World, owner: Owner, pos: Position);
    /// Despawn.
    fn despawn(w: &mut Self::World, owner: Owner);
}

/// bevy_ecs game side.
#[derive(Default)]
pub struct BevySide;

impl GameSide for BevySide {
    type World = bevy_ecs::world::World;
    type Feed = BevyCellFeed<Position, Position>;
    fn world() -> Self::World {
        bevy_ecs::world::World::new()
    }
    fn spawn(w: &mut Self::World, pos: Position, wire: WireId) -> Owner {
        k0_kit_bevy_ecs::EntityExt::to_owner(w.spawn((pos, KitWire(wire))).id())
    }
    #[inline]
    fn write(w: &mut Self::World, owner: Owner, pos: Position) {
        if let Some(mut p) = w.get_mut::<Position>(k0_kit_bevy_ecs::OwnerExt::entity(owner)) {
            *p = pos;
        }
    }
    fn despawn(w: &mut Self::World, owner: Owner) {
        w.despawn(k0_kit_bevy_ecs::OwnerExt::entity(owner));
    }
}

/// hecs game side over `Tracked<Position>`.
#[derive(Default)]
pub struct HecsTrackedSide;

impl GameSide for HecsTrackedSide {
    type World = hecs::World;
    type Feed = HecsTrackedFeed<Position>;
    fn world() -> Self::World {
        hecs::World::new()
    }
    fn spawn(w: &mut Self::World, pos: Position, wire: WireId) -> Owner {
        k0_kit_hecs::EntityExt::to_owner(w.spawn((Tracked::new(pos), wire)))
    }
    #[inline]
    fn write(w: &mut Self::World, owner: Owner, pos: Position) {
        if let Some(e) = k0_kit_hecs::OwnerExt::entity(owner)
            && let Ok(p) = w.query_one_mut::<&mut Tracked<Position>>(e)
        {
            **p = pos;
        }
    }
    fn despawn(w: &mut Self::World, owner: Owner) {
        if let Some(e) = k0_kit_hecs::OwnerExt::entity(owner) {
            let _ = w.despawn(e);
        }
    }
}

/// hecs game side over a plain `Position` (no change source).
#[derive(Default)]
pub struct HecsDiffSide;

impl GameSide for HecsDiffSide {
    type World = hecs::World;
    type Feed = HecsDiffFeed<Position>;
    fn world() -> Self::World {
        hecs::World::new()
    }
    fn spawn(w: &mut Self::World, pos: Position, wire: WireId) -> Owner {
        k0_kit_hecs::EntityExt::to_owner(w.spawn((pos, wire)))
    }
    #[inline]
    fn write(w: &mut Self::World, owner: Owner, pos: Position) {
        if let Some(e) = k0_kit_hecs::OwnerExt::entity(owner)
            && let Ok(p) = w.query_one_mut::<&mut Position>(e)
        {
            *p = pos;
        }
    }
    fn despawn(w: &mut Self::World, owner: Owner) {
        if let Some(e) = k0_kit_hecs::OwnerExt::entity(owner) {
            let _ = w.despawn(e);
        }
    }
}

/// The agnostic core driven through a binding.
pub struct Agnostic<G: GameSide> {
    world: G::World,
    feed: G::Feed,
    book: CellBook<P2, Cell>,
    space: Grid2,
    minter: Minter,
    slots: Vec<Option<(Owner, bool)>>,
    gone: Vec<Gone>,
}

impl<G: GameSide> Agnostic<G> {
    /// A fresh driver with `slots` logical slots.
    #[must_use]
    pub fn new(slots: usize) -> Self {
        Self {
            world: G::world(),
            feed: G::Feed::default(),
            book: CellBook::default(),
            space: Grid2::new(CELL_SIZE),
            minter: Minter::default(),
            slots: vec![None; slots],
            gone: Vec::new(),
        }
    }
}

impl<G: GameSide> Driver for Agnostic<G> {
    fn apply(&mut self, ops: &[Op]) {
        for op in ops {
            match *op {
                Op::Spawn { slot, pos, member } => {
                    let owner = G::spawn(&mut self.world, pos, self.minter.mint());
                    if member {
                        self.book.add_member(owner);
                    }
                    self.slots[slot] = Some((owner, member));
                }
                Op::Move { slot, pos } => {
                    if let Some((owner, _)) = self.slots[slot] {
                        G::write(&mut self.world, owner, pos);
                    }
                }
                Op::Despawn { slot } => {
                    if let Some((owner, member)) = self.slots[slot].take() {
                        if member {
                            self.book.park_leave(owner);
                        }
                        G::despawn(&mut self.world, owner);
                    }
                }
            }
        }
    }

    #[inline]
    fn update(&mut self) {
        run_tick(
            &mut self.book,
            &self.space,
            &mut self.feed,
            &mut self.world,
            &mut self.gone,
        );
    }

    fn canon(&self) -> Canon {
        canon(
            self.book.buckets.iter(),
            self.book.cell_changes.iter(),
            self.book.born_groups.iter(),
            self.book.member_counts.iter(),
        )
    }

    fn packets(&self, tick: u64) -> Option<Vec<(Cell, Vec<u8>)>> {
        let cells: BTreeSet<Cell> = self.book.member_counts.keys().copied().collect();
        Some(
            cells
                .into_iter()
                .filter_map(|c| {
                    let mut out = Vec::new();
                    assemble_group_packet(&self.book, &self.space, c, tick, &mut out)
                        .then_some((c, out))
                })
                .collect(),
        )
    }
}
