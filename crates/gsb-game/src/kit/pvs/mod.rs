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
//! ## The map
//!
//! The map — the convex sectors, the static visibility table and the
//! out-of-map sector — is game data (KIT-ARCHITECTURE §2: map data is the
//! game's), so it lives with the demo game and reaches this room through
//! the seam (`sector_of`, `VISIBLE_FROM`, `Sector`, `SECTOR_OUT`; the
//! future `SectorMap`, §4.2). The room only does the two lookups:
//! `sector_of(position)` and `VISIBLE_FROM[sector]`.
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

use bevy_ecs::prelude::Entity;
use gsb_core::id::PlayerId;
use gsb_ecs::SystemRunner;

use crate::kit::seam;
use crate::kit::seam::{SECTOR_OUT, Sector, VISIBLE_FROM, sector_of};

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
    minter: crate::kit::identity::Minter,
    /// Half-size of the square spawn map (see the demo's `spawn_pos`).
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
    /// the demo's `ingest` / `crate::kit::common::emit_private`).
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
        Self::with_spawn_half(seam::DEFAULT_SPAWN_HALF)
    }

    /// Build a PVS room whose spawn map has half-size `half` (see the
    /// `spawn_half` field docs for what that means on the fixed PVS map).
    #[must_use]
    pub fn with_spawn_half(half: f32) -> Self {
        Self {
            runner: seam::movement_runner(),
            player_entity: HashMap::new(),
            next_player_id: 0,
            park: crate::kit::common::ParkPolicy::default(),
            park_ledger: HashMap::new(),
            minter: crate::kit::identity::Minter::sequential(),
            spawn_half: half.max(1.0),
            last: HashMap::new(),
            buckets: HashMap::new(),
            input: HashMap::new(),
            encoded: 0,
        }
    }

    /// Set the disconnect-park grace (see
    /// [`crate::kit::room::OpenRoom::with_disconnect_grace`]; RECONNECT §3).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.park.grace = grace;
        self
    }
}

// Faz 1 trait split: shared hooks on the `GameLogic` supertrait; no
// room-exclusive hook used (empty `RoomLogic` impl at the bottom).
