//! [`SectorRoom`]: per-map-segment (PVS) game logic, generic over the
//! game (`G: Game`) and the map (`M: SectorMap`, KIT-ARCHITECTURE §4.2).
//! The sections below speak in the demo's terms (its four-sector map
//! through the kit's `ConvexSectors2` preset).
//!
//! ## What it changes (and what it deliberately does not touch)
//!
//! Visibility here is **not distance at all** — it is the map's own
//! geometry. The map is a set of hand-defined **convex sectors** plus
//! the transitions between them, and a **precomputed static visibility
//! table**: which sectors see which sectors. An entity's group is the
//! sector containing its position (`GroupKey = Sector` — deliberately
//! *not* `RoomId`, which already means
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
//! point-in-convex test) and then `visible_from(sector)` (a table read),
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
//! game's), so the room takes it as a [`SectorMap`] value (the demo: its
//! hand-authored polygons and table in the kit's `ConvexSectors2`
//! preset). The room only does the two lookups: `sector_of(position)`
//! and `visible_from(sector)`.
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

use crate::kit::common::{InputSeq, ParkEntry, ParkPolicy};
use crate::kit::game::{Game, Wire};
use crate::kit::identity::Minter;
use crate::kit::space::SectorMap;
// The fixture map's out-of-map sector and the preset's sector key, in
// scope for the in-module tests (they address sectors directly through
// `use super::*`).
#[cfg(test)]
use crate::kit::space::Sector;
#[cfg(test)]
use crate::kit::testing::SECTOR_OUT;

/// The PVS room: sector group key, static-table visibility, per-sector
/// "no change" ledger.
pub struct SectorRoom<G: Game, M: SectorMap> {
    /// The game (its hooks, its codec and its own state).
    game: G,
    /// The map: sectors + static visibility table (game data).
    map: M,
    /// Which entity belongs to which player (Faz 2: keyed by the STABLE
    /// player identity — the mapping survives resume unchanged).
    player_entity: HashMap<PlayerId, Entity>,
    /// The player-identity counter (the room's [`PlayerId`] minting
    /// policy); monotonic, never reused within the room's lifetime.
    next_player_id: u64,
    /// The disconnect-park policy + ledger (see `crate::kit::common` and
    /// RECONNECT §3/§9; the hook bodies are shared with every room).
    park: ParkPolicy,
    park_ledger: HashMap<String, ParkEntry>,
    /// The room's single wire-identity counter (mirrors the other rooms).
    minter: Minter,
    /// Per-sector "no change" ledger: `sector → (wire id → wire value)`,
    /// the exact wire content of that sector's last emitted snapshot.
    /// Keyed by group (sector) per the [`GameLogic::snapshot`] contract.
    last: HashMap<M::Sector, HashMap<u64, Wire<G>>>,
    /// Per-tick bucket cache, rebuilt in [`Self::update`]: `sector →
    /// [(wire id, wire value)]`. Each entity is bucketed **once** per
    /// tick; a sector's snapshot is the union of the buckets of the
    /// sectors the map says are visible from it, assembled without
    /// re-querying the world.
    buckets: HashMap<M::Sector, Vec<(u64, Wire<G>)>>,
    /// Per-player input sequence state (strategy-independent; see
    /// `crate::kit::common::emit_private`).
    input: InputSeq,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `GameLogic::encoded_records`).
    encoded: u64,
}

impl<G: Game, M: SectorMap> SectorRoom<G, M> {
    /// Build a PVS room running `game` over the map `map`.
    #[must_use]
    pub fn with_game(game: G, map: M) -> Self {
        Self {
            game,
            map,
            player_entity: HashMap::new(),
            next_player_id: 0,
            park: ParkPolicy::default(),
            park_ledger: HashMap::new(),
            minter: Minter::sequential(),
            last: HashMap::new(),
            buckets: HashMap::new(),
            input: InputSeq::default(),
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

    /// The game this room runs.
    pub fn game(&self) -> &G {
        &self.game
    }

    /// The game this room runs, for configuration after construction.
    pub fn game_mut(&mut self) -> &mut G {
        &mut self.game
    }
}

// Faz 1 trait split: shared hooks on the `GameLogic` supertrait; no
// room-exclusive hook used (empty `RoomLogic` impl at the bottom).
