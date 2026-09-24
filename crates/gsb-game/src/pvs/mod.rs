//! [`SectorRoom`]: per-map-segment (PVS) game logic for the demo game.
//!
//! ## What it changes (and what it deliberately does not touch)
//!
//! Visibility here is **not distance at all** — it is the map's own
//! geometry. The map is a set of hand-defined **convex sectors**
//! ([`SECTORS`]) plus the transitions between them, and a **precomputed
//! static visibility table** ([`VISIBLE_FROM`]): which sectors see which
//! sectors. An entity's group is the sector containing its position
//! (`GroupKey = Sector` — deliberately *not* `RoomId`, which already means
//! a gsb room; a sector is a region *inside* a room); a sector's snapshot
//! is the union of the entities of every sector the table says is visible
//! from it. The room's *existing* machinery then produces one snapshot per
//! sector and shares the `Bytes` with that sector's occupants — the same
//! "compute once per group, share by reference" property as `AoiRoom`.
//! **No `gsb-core` change.**
//!
//! **Why a static table lookup is the point.** A BSP compiler is *not*
//! written on purpose: for this demo the hand-written table *is* the
//! precomputation a real PVS pipeline (BSP/portal graphs) would produce.
//! What the seam proves is that "visibility" is a *static lookup over
//! hand-authored map data* — the runtime does `sector_of(position)` (a
//! point-in-convex test) and then `VISIBLE_FROM[sector]` (a table read),
//! never a distance comparison. That is what separates PVS from distance
//! AOI: two entities **3 units apart** do not see each other when their
//! sectors are unlinked in the table (a wall), and entities 60 units apart
//! DO see each other when their sectors are linked (a long sightline).
//! `tests/pvs.rs` pins both directions.
//!
//! ## The map (hand-written, convex, tiles the [-50, 50]² arena)
//!
//! ```text
//!        C | D            y
//!  -------+------  x=-10
//!   50    |      (north: C sees A and D; D sees B and C —
//!         |       an open north sightline)
//! -------A------B-----  y=20   (A↔C and B↔D are open passages)
//!  -50 -10|  x=0
//!         |            (A and B are GEOMETRICALLY adjacent — 3 units
//!   -50   |            apart at the closest — but the table says they
//!                 do NOT see each other: a wall. Distance-based AOI
//!                 cannot express this.)
//! ```
//!
//! - `A` (west), `B` (east): the two lobbies, split by a wall at `x = 0`
//!   (`y ∈ [-50, 20]`).
//! - `C` (northwest), `D` (northeast): the north band (`y ∈ [20, 50]`),
//!   split at `x = -10`.
//! - The sectors tile the arena plane, so every arena position is in
//!   exactly one sector (shared edges go to the first sector in the fixed
//!   test order — deterministic). A position outside every sector (a
//!   client sending a target beyond the map) lands in [`SECTOR_OUT`],
//!   which sees only itself: the broadcast set stays "has a `Position`"
//!   and a runaway entity can never leak into the map's visibility.
//!
//! ## Alternatives considered and rejected
//!
//! - *A real BSP/portal-graph compiler* — out of scope by the spec's own
//!   words ("for the demo a hand-written sector/transition table is
//!   enough"); it would add a build step and a second map format for no
//!   seam benefit: the runtime lookup shape (static table) is identical.
//! - *Axis-aligned rectangles only* — less expressive than the spec's
//!   "convex regions", and the general convex-polygon test is ~20 lines
//!   (`in_convex`), so restricting the geometry buys nothing.
//! - *A spatial index over the sectors* — unnecessary at this sector
//!   count (4 + OUT): the direct point-in-polygon test over the fixed
//!   list is clearer, and the lookup cost is O(sectors × edges) per
//!   entity per tick, which is negligible next to encoding/fan-out
//!   (measured in the load test).
//!
//! ## Invariants preserved (see `tests/pvs.rs` and the inline tests)
//!
//! - **Identity**: the wire id is minted once (`on_join` / orphan stamp in
//!   `update`) and never changes; a sector-changing entity keeps it.
//! - **Late join**: a joiner's entity enters the world in the control
//!   phase, so it is in the sector snapshot that its sector emits on the
//!   first broadcast — the joiner sees its sector's full visibility set.
//! - **Broadcast set**: exactly "has a `Position`" (orphan stamping in
//!   `update`, structural like `OpenRoom`).
//! - **Self-contained**: no delta, no history; the per-sector ledger
//!   compares exactly the wire content of that sector's last emitted
//!   snapshot (per-group bookkeeping contract), so an entity crossing a
//!   sector boundary *moves its record* between snapshots and the client
//!   reads "moved" from the full replacements alone.

mod logic;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::hash::Hash;

use bevy_ecs::prelude::Entity;
use gsb_core::id::PlayerId;
use gsb_ecs::SystemRunner;

use crate::components::Position;

/// A map sector — the PVS group key (a region *inside* a room; deliberately
/// not `RoomId`, which names a gsb room).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sector(pub u8);

/// The hand-written sectors of the demo map (module docs, "The map"):
/// convex polygons, counter-clockwise, tiling the [-50, 50]² arena.
const SECTOR_WEST: u8 = 0;
/// `A`: x ∈ [-50, 0], y ∈ [-50, 20].
const SECTOR_EAST: u8 = 1;
/// `B`: x ∈ [0, 50], y ∈ [-50, 20].
const SECTOR_NW: u8 = 2;
/// `C`: x ∈ [-50, -10], y ∈ [20, 50].
const SECTOR_NE: u8 = 3;
/// `D`: x ∈ [-10, 50], y ∈ [20, 50].
/// Positions outside every hand-written sector (a target beyond the map).
pub const SECTOR_OUT: u8 = 4;

/// The sector polygons (index = sector id, counter-clockwise winding).
const SECTORS: [[(f32, f32); 4]; 4] = [
    // A (west)
    [(-50.0, -50.0), (0.0, -50.0), (0.0, 20.0), (-50.0, 20.0)],
    // B (east)
    [(0.0, -50.0), (50.0, -50.0), (50.0, 20.0), (0.0, 20.0)],
    // C (northwest)
    [(-50.0, 20.0), (-10.0, 20.0), (-10.0, 50.0), (-50.0, 50.0)],
    // D (northeast)
    [(-10.0, 20.0), (50.0, 20.0), (50.0, 50.0), (-10.0, 50.0)],
];

/// The precomputed **static** visibility table: for each sector, a bitmask
/// of the sectors visible *from* it (itself included). This is the PVS —
/// the hand-authored map geometry (transitions/sightlines) frozen into a
/// table the runtime only reads.
///
/// - `A` sees `A` and `C` (open passage north of the west lobby).
/// - `B` sees `B` and `D` (open passage north of the east lobby).
/// - `C` sees `A`, `C` and `D` (plus the open north sightline to `D`).
/// - `D` sees `B`, `C` and `D`.
/// - `A` and `B` see NEITHER each other: they are geometrically adjacent
///   (3 units apart at the closest) but separated by a wall — the table
///   is the law, not the distance.
/// - `OUT` sees only itself (a runaway entity leaks into nothing).
const VISIBLE_FROM: [u16; 5] = [
    1 << SECTOR_WEST | 1 << SECTOR_NW,
    1 << SECTOR_EAST | 1 << SECTOR_NE,
    1 << SECTOR_WEST | 1 << SECTOR_NW | 1 << SECTOR_NE,
    1 << SECTOR_EAST | 1 << SECTOR_NW | 1 << SECTOR_NE,
    1 << SECTOR_OUT,
];

/// True when `(x, y)` is inside (or on the edge of) a counter-clockwise
/// convex polygon. A point is inside a convex polygon iff all edge
/// cross products have the same sign (edge points count as inside, so
/// shared sector boundaries are owned by the first sector in test order —
/// deterministic).
fn in_convex(poly: &[(f32, f32); 4], x: f32, y: f32) -> bool {
    let mut sign = 0.0f32;
    let n = poly.len();
    for i in 0..n {
        let (ax, ay) = poly[i];
        let (bx, by) = poly[(i + 1) % n];
        let cross = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
        if cross.abs() < 1e-9 {
            continue; // on the edge
        }
        let s = cross.signum();
        if sign == 0.0 {
            sign = s;
        } else if s != sign {
            return false;
        }
    }
    true
}

/// The sector containing `pos`: the first (fixed order) sector whose
/// polygon contains it, or [`SECTOR_OUT`] when outside all of them.
#[inline]
fn sector_of(pos: Position) -> Sector {
    for (i, poly) in SECTORS.iter().enumerate() {
        if in_convex(poly, pos.x, pos.y) {
            return Sector(i as u8);
        }
    }
    Sector(SECTOR_OUT)
}

/// The PVS room: sector group key, static-table visibility, per-sector
/// "no change" ledger.
pub struct SectorRoom {
    runner: SystemRunner,
    /// Which entity belongs to which player (Faz 2: keyed by the STABLE
    /// player identity — the mapping survives resume unchanged).
    player_entity: HashMap<PlayerId, Entity>,
    /// The player-identity counter (the demo's [`PlayerId`] minting
    /// policy); monotonic, never reused within the room's lifetime.
    next_player_id: u64,
    /// The disconnect-park policy + ledger (see `crate::kit::common` and
    /// RECONNECT §3/§9; the hook bodies are shared with every demo room).
    park: crate::kit::common::ParkPolicy,
    park_ledger: HashMap<String, crate::kit::common::ParkEntry>,
    /// The room's single wire-identity counter (mirrors the other rooms).
    next_wire_id: u64,
    /// Half-size of the square spawn map (see `gsb_game::room::spawn_pos`).
    /// The demo *PVS map* stays the hand-authored 100×100 sectors; this
    /// only affects where `on_join` places entities (a `spread`-profile
    /// run places them outside every sector — they land in
    /// [`SECTOR_OUT`] and see only themselves, which is exactly what the
    /// PVS strategy promises for off-map positions).
    spawn_half: f32,
    /// Per-sector "no change" ledger: `sector → (wire id → (x, y))`, the
    /// exact wire content of that sector's last emitted snapshot. Keyed by
    /// group (sector) per the [`GameLogic::snapshot`] contract.
    last: HashMap<Sector, HashMap<u64, (i32, i32)>>,
    /// Per-tick bucket cache, rebuilt in [`Self::update`]: `sector →
    /// [(wire id, x, y)]`. Each entity is bucketed **once** per tick; a
    /// sector's snapshot is the union of the buckets of the sectors
    /// [`VISIBLE_FROM`] says are visible from it, assembled by reference
    /// without re-querying the world.
    buckets: HashMap<Sector, Vec<(u64, i32, i32)>>,
    /// Per-player input sequence state (strategy-independent; see
    /// `crate::kit::common::ingest` / `emit_private`).
    input: HashMap<PlayerId, crate::kit::common::InputState>,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `GameLogic::encoded_records`).
    encoded: u64,
}

impl Default for SectorRoom {
    fn default() -> Self {
        Self::new()
    }
}

impl SectorRoom {
    /// Build a PVS room over the demo map (module docs, "The map") and the
    /// default 100×100 spawn arena.
    #[must_use]
    pub fn new() -> Self {
        Self::with_spawn_half(crate::room::DEFAULT_SPAWN_HALF)
    }

    /// Build a PVS room whose spawn map has half-size `half` (see the
    /// `spawn_half` field docs for what that means on the fixed PVS map).
    #[must_use]
    pub fn with_spawn_half(half: f32) -> Self {
        Self {
            runner: crate::kit::seam::movement_runner(),
            player_entity: HashMap::new(),
            next_player_id: 0,
            park: crate::kit::common::ParkPolicy::default(),
            park_ledger: HashMap::new(),
            next_wire_id: 0,
            spawn_half: half.max(1.0),
            last: HashMap::new(),
            buckets: HashMap::new(),
            input: HashMap::new(),
            encoded: 0,
        }
    }

    /// Set the disconnect-park grace (see
    /// [`crate::room::OpenRoom::with_disconnect_grace`]; RECONNECT §3).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.park.grace = grace;
        self
    }
}

// Faz 1 trait split: shared hooks on the `GameLogic` supertrait; no
// room-exclusive hook used (empty `RoomLogic` impl at the bottom).
