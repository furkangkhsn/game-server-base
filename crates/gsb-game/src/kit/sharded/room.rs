//! The whole-world sharded room: N shard actors over one map, every
//! shard broadcasting its own slice to everyone in it.

use std::collections::{HashMap, HashSet};

use bevy_ecs::prelude::Entity;
use gsb_core::id::PlayerId;
use gsb_core::shard::{BorderRecord, SHARD_SERIAL_RANGE};
use gsb_ecs::SystemRunner;

use crate::kit::seam;
use crate::kit::seam::{EconomyService, Position};
use crate::kit::sharded::*;

mod logic;
mod shard;

/// The sharded-room shard logic: one shard of the grid (see module docs).
pub struct ShardedRoom {
    pub(in crate::kit::sharded) runner: SystemRunner,
    /// This shard's index in the grid (row-major: `row * cols + col`).
    pub(in crate::kit::sharded) index: usize,
    pub(in crate::kit::sharded) shard_count: usize,
    /// The grid's column count (used by [`Self::rect`]).
    pub(in crate::kit::sharded) cols: usize,
    /// Half-size of the square map (shared by all shards — the factory
    /// builds every shard with the same `spawn_half`).
    pub(in crate::kit::sharded) half: f32,
    pub(in crate::kit::sharded) cell_w: f32,
    pub(in crate::kit::sharded) cell_h: f32,
    /// Boundary margin (module docs, "Visibility model"): a quarter of the
    /// smallest cell dimension. Entities within this of a shared edge are
    /// exported to the neighbor and appear in the neighbor's snapshots.
    pub(in crate::kit::sharded) border: f32,
    /// The 4-neighborhood of this shard in the grid (indices), in stable
    /// order (west, east, north, south — the core sends border/migrate to
    /// exactly these).
    pub(in crate::kit::sharded) neighbors: Vec<usize>,
    /// Player → entity (this shard's players; Faz 2: keyed by the STABLE
    /// player identity, which survives resume AND migration unchanged).
    pub(in crate::kit::sharded) player_entity: HashMap<PlayerId, Entity>,
    /// The disconnect-park policy (see `crate::kit::common`; RECONNECT §3).
    pub(in crate::kit::sharded) park: crate::kit::common::ParkPolicy,
    /// The park ledger of THIS shard's parked players (§4: it lives in
    /// the logic; §14.2: records travel with migrations inside
    /// [`ShardedRoomState`], so a detached entity crossing a seam is
    /// parked on the receiving shard, never stranded on the old one).
    pub(in crate::kit::sharded) park_ledger: HashMap<String, ShardParkRecord>,
    /// Entity → owning player (only entities owned by a player).
    pub(in crate::kit::sharded) entity_player: HashMap<Entity, PlayerId>,
    /// Wire id → entity (every entity, for migrate-out despawn).
    pub(in crate::kit::sharded) wire_entity: HashMap<u64, Entity>,
    /// How many identities this shard has minted (the range is
    /// `index * SHARD_SERIAL_RANGE + serial_used`). BOTH identity spaces
    /// draw from this one counter — wire ids AND stable player ids — so
    /// the core's range-exhaustion guard stays exact over everything the
    /// range backs.
    pub(in crate::kit::sharded) serial_used: u64,
    /// The wire ids this shard currently owns, kept in sync on every
    /// mutation (join/leave/migrate-in/out). `own_wires` takes `&World`
    /// (it cannot query), so it reads this set instead. Accuracy matters
    /// for the core's duplicate filter: a stale entry for an entity that
    /// just migrated out would hide the neighbor's (now-correct) record
    /// of it, dropping it from this shard's view for a tick.
    pub(in crate::kit::sharded) own_wires: HashSet<u64>,
    /// This shard's boundary records (module docs, "Visibility model"),
    /// rebuilt at the end of `update` (positions change in the movement
    /// system). `collect_border` takes `&World` (it cannot query), so it
    /// returns a clone of this cache. It is one tick stale with respect
    /// to the phase-4 migrate-out despawns (an entity that just crossed
    /// out is still exported) — harmless: the receiving shard owns it
    /// now and its own-wires filter drops the stale copy (own wins).
    pub(in crate::kit::sharded) border_cache: Vec<BorderRecord<StripPos>>,
    /// The wire content of the last emitted snapshot (single group,
    /// `GroupKey = ()`): `wire → (x, y)` (truncated).
    pub(in crate::kit::sharded) last: HashMap<u64, (i32, i32)>,
    /// Entity records encoded during the most recent broadcast phase.
    pub(in crate::kit::sharded) encoded: u64,
    /// Per-player input sequence state (strategy-independent; see
    /// the demo's `ingest` / `crate::kit::common::emit_private`). The
    /// session stays bound to this shard even if its entity migrates (its
    /// input is routed through this shard's room), so the session lives
    /// here.
    pub(in crate::kit::sharded) input: HashMap<PlayerId, crate::kit::common::InputState>,
    /// The economy service handle (the RPC pattern's external-I/O half on
    /// the SHARDED path — Faz 3; the demo's economy service); `None` = this
    /// shard answers `ECONOMY` requests with a normal "not configured"
    /// rejection. One service per server, shared by clone with every
    /// shard (the platform's economy is not a per-shard thing).
    pub(in crate::kit::sharded) economy: Option<EconomyService>,
}

impl ShardedRoom {
    /// Build shard `index` of a `shard_count`-shard room over a square
    /// map of half-size `half`. All shards of a room share `half`.
    pub fn new(index: usize, shard_count: usize, spawn_half: f32) -> Self {
        let (rows, cols) = grid_shape(shard_count);
        let half = spawn_half.max(1.0);
        let cell_w = 2.0 * half / cols as f32;
        let cell_h = 2.0 * half / rows as f32;
        let row = index / cols;
        let col = index % cols;
        let mut neighbors = Vec::with_capacity(4);
        if col > 0 {
            neighbors.push(index - 1);
        }
        if col + 1 < cols {
            neighbors.push(index + 1);
        }
        if row > 0 {
            neighbors.push(index - cols);
        }
        if row + 1 < rows {
            neighbors.push(index + cols);
        }
        Self {
            runner: seam::movement_runner(),
            index,
            shard_count,
            cols,
            half,
            cell_w,
            cell_h,
            border: (cell_w.min(cell_h)) / 4.0,
            neighbors,
            player_entity: HashMap::new(),
            park: crate::kit::common::ParkPolicy::default(),
            park_ledger: HashMap::new(),
            entity_player: HashMap::new(),
            wire_entity: HashMap::new(),
            serial_used: 0,
            own_wires: HashSet::new(),
            border_cache: Vec::new(),
            last: HashMap::new(),
            encoded: 0,
            input: HashMap::new(),
            economy: None,
        }
    }

    /// Set the disconnect-park grace (see
    /// [`crate::kit::room::OpenRoom::with_disconnect_grace`]; RECONNECT §3).
    /// Every shard of a room should carry the same policy (the factory
    /// builds them uniformly).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.park.grace = grace;
        self
    }

    /// Attach the economy service handle (the RPC pattern's external-I/O
    /// half on the sharded path — Faz 3; see [`Self::economy`]). Builder-
    /// style, like [`crate::kit::room::OpenRoom::with_economy`]; every shard of
    /// a room gets a clone of the ONE server-wide service.
    #[must_use]
    pub fn with_economy(mut self, economy: EconomyService) -> Self {
        self.economy = Some(economy);
        self
    }

    /// This shard's region rectangle `[x0, x1] × [y0, y1]`.
    fn rect(&self) -> (f32, f32, f32, f32) {
        let row = self.index / self.cols;
        let col = self.index % self.cols;
        let x0 = -self.half + col as f32 * self.cell_w;
        let y0 = -self.half + row as f32 * self.cell_h;
        (x0, x0 + self.cell_w, y0, y0 + self.cell_h)
    }

    /// Mint the next identity from this shard's disjoint range (both
    /// wire ids and stable player ids draw from the ONE counter — see
    /// the `serial_used` field docs).
    fn mint(&mut self) -> u64 {
        self.serial_used += 1;
        self.index as u64 * SHARD_SERIAL_RANGE + self.serial_used
    }

    /// Mint the next STABLE player identity (Faz 2): range-partitioned
    /// like the wire ids, so two shards never mint the same player.
    fn mint_player(&mut self) -> PlayerId {
        PlayerId(self.mint())
    }

    /// Whether the (truncated) position `(x, y)` lies in the border frame
    /// of this shard's region (within `border` of the region rectangle,
    /// including the thin overlap into it): the filter that decides which
    /// borrowed records from the neighbors this shard's snapshots include
    /// (module docs, "Visibility model").
    fn in_border_frame(&self, x: i32, y: i32) -> bool {
        let (x0, x1, y0, y1) = self.rect();
        let b = self.border;
        x as f32 >= x0 - b && x as f32 <= x1 + b && y as f32 >= y0 - b && y as f32 <= y1 + b
    }
}

// Faz 1 trait split: the shared contract — snapshot groups, the tick
// seam, membership, the reconnect surface — implements the `GameLogic`

/// The region (shard index) of a position for this shard's grid.
impl ShardedRoom {
    fn region_of(&self, pos: Position) -> usize {
        shard_at(pos.x, pos.y, self.half, self.shard_count)
    }
}
