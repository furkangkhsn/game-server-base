//! The demo's rooms: the kit's generic strategy rooms instantiated with
//! [`DemoGame`], plus the constructors every consumer (`gsb-server`'s
//! factories, the load generator, the tests) has always called. The
//! crate root's compatibility paths (`gsb_demo::room::OpenRoom`, …) are
//! type aliases onto these instantiations.
//!
//! **The constructors are extension traits** (`OpenRoomExt`, …; all in
//! [`crate::prelude`]). The room types are the kit's, so an inherent
//! `impl OpenRoom<DemoGame> { fn new() … }` is legal only while kit and
//! demo share a crate — across the crate split it is E0116. A trait per
//! room keeps every call site's syntax (`OpenRoom::new()`,
//! `AoiRoom::with_spawn_half(c, h).with_disconnect_grace(g)`,
//! `.with_economy(e)` in the builder chain); a consumer adds one
//! `use gsb_demo::prelude::*;`. Rejected: free functions (every call
//! site rewritten, and `with_economy` would break the builder chain
//! into a nested call); newtypes around the kit rooms (every
//! `GameLogic` / `ShardLogic` method delegated, six times over);
//! constructors generic over the game in the kit (the kit cannot know
//! the demo's spawn map, map data or economy service). The old
//! `Default` impls of the open and PVS rooms are gone for the same
//! reason (a foreign trait on a foreign type) — nothing called them.

use crate::demo::components::Position;
use crate::demo::economy::EconomyService;
use crate::demo::play::DemoGame;
use crate::demo::sectors::demo_map;
use crate::demo::spawn::DEFAULT_SPAWN_HALF;
use crate::kit::aoi::AoiRoom;
use crate::kit::pvs::SectorRoom;
use crate::kit::room::OpenRoom;
use crate::kit::sharded::{ShardedRoom, ShardedSpatialRoom};
use crate::kit::space::{ConvexSectors2, Grid2, GridPartition2, VisionGrid2};
use crate::kit::team::TeamRoom;

/// The demo's constructors for the open-visibility room
/// (`OpenRoom<DemoGame>`).
pub trait OpenRoomExt: Sized {
    /// Build the open room over the default 100×100 arena (bit-identical
    /// spawn distribution to the pre-config rooms).
    #[must_use]
    fn new() -> Self;

    /// Build the open room over a square spawn map of half-size `half`
    /// (entities spawn uniformly in `[-half, half]²`). The load
    /// generator's `spread` profile pairs this with its home distribution
    /// so spawn points and targets live on the same (possibly "wide")
    /// map.
    #[must_use]
    fn with_spawn_half(half: f32) -> Self;

    /// Attach the economy service handle (the RPC pattern's external-I/O
    /// half; the demo's economy service). The room delegates `ECONOMY`
    /// requests to it; the answer arrives on a later tick through the
    /// room's completion channel.
    #[must_use]
    fn with_economy(self, economy: EconomyService) -> Self;
}

impl OpenRoomExt for OpenRoom<DemoGame> {
    fn new() -> Self {
        Self::with_spawn_half(DEFAULT_SPAWN_HALF)
    }

    fn with_spawn_half(half: f32) -> Self {
        Self::with_game(DemoGame::new(half))
    }

    fn with_economy(mut self, economy: EconomyService) -> Self {
        self.game_mut().set_economy(economy);
        self
    }
}

/// The demo's constructors for its AOI: the kit's 2D grid preset over
/// the demo's truncated `(x, y)` wire value (`AoiRoom<DemoGame, Grid2>`).
pub trait AoiRoomExt: Sized {
    /// Build an AOI room with the given `cell_size` (world units per cell
    /// edge) over the default 100×100 spawn arena. Clamped to a sane
    /// minimum so a degenerate `0` cannot produce a single infinite cell.
    #[must_use]
    fn new(cell_size: f32) -> Self;

    /// Build an AOI room over a square spawn map of half-size `half` (see
    /// [`OpenRoomExt::with_spawn_half`]).
    #[must_use]
    fn with_spawn_half(cell_size: f32, half: f32) -> Self;
}

impl AoiRoomExt for AoiRoom<DemoGame, Grid2> {
    fn new(cell_size: f32) -> Self {
        Self::with_spawn_half(cell_size, DEFAULT_SPAWN_HALF)
    }

    fn with_spawn_half(cell_size: f32, half: f32) -> Self {
        Self::with_game(DemoGame::new(half), Grid2::new(cell_size))
    }
}

/// The demo's constructors for its team fog: the kit's 2D vision preset
/// over the demo's `Position`, a uniform vision radius on the map plane
/// (`TeamRoom<DemoGame, VisionGrid2<Position>>`).
pub trait TeamRoomExt: Sized {
    /// Build a team-fog room with the given `vision_radius` (world units)
    /// over the default 100×100 spawn arena. Clamped to a sane minimum so a
    /// degenerate `0` cannot make vision "only the exact same point".
    #[must_use]
    fn new(vision_radius: f32) -> Self;

    /// Build a team-fog room over a square spawn map of half-size `half`
    /// (see [`OpenRoomExt::with_spawn_half`]).
    #[must_use]
    fn with_spawn_half(vision_radius: f32, half: f32) -> Self;
}

impl TeamRoomExt for TeamRoom<DemoGame, VisionGrid2<Position>> {
    fn new(vision_radius: f32) -> Self {
        Self::with_spawn_half(vision_radius, DEFAULT_SPAWN_HALF)
    }

    fn with_spawn_half(vision_radius: f32, half: f32) -> Self {
        Self::with_game(DemoGame::new(half), VisionGrid2::new(vision_radius))
    }
}

/// The demo's constructors for its PVS: its hand-authored four-sector
/// map (`demo_map`) in the kit's convex-sector preset over the demo's
/// `Position` (`SectorRoom<DemoGame, ConvexSectors2<Position>>`).
pub trait SectorRoomExt: Sized {
    /// Build a PVS room over the demo map and the default 100×100 spawn
    /// arena.
    #[must_use]
    fn new() -> Self;

    /// Build a PVS room whose spawn map has half-size `half`. The demo
    /// *PVS map* stays the hand-authored 100×100 sectors; this only
    /// affects where joins place entities (a `spread`-profile run places
    /// them outside every sector — they land in the containment sector
    /// and see only themselves, which is exactly what the PVS strategy
    /// promises for off-map positions).
    #[must_use]
    fn with_spawn_half(half: f32) -> Self;
}

impl SectorRoomExt for SectorRoom<DemoGame, ConvexSectors2<Position>> {
    fn new() -> Self {
        Self::with_spawn_half(DEFAULT_SPAWN_HALF)
    }

    fn with_spawn_half(half: f32) -> Self {
        Self::with_game(DemoGame::new(half), demo_map())
    }
}

/// The demo's constructors for its sharded world: the kit's 2D grid
/// partition over the demo's `Position` (and its `StripPos` wire value
/// for the border frame) (`ShardedRoom<DemoGame, GridPartition2<Position>>`).
pub trait ShardedRoomExt: Sized {
    /// Build shard `index` of a `shard_count`-shard room over a square
    /// map of half-size `spawn_half`. All shards of a room share it.
    #[must_use]
    fn new(index: usize, shard_count: usize, spawn_half: f32) -> Self;

    /// Attach the economy service handle (the RPC pattern's external-I/O
    /// half on the sharded path — Faz 3). Builder-style, like
    /// [`OpenRoomExt::with_economy`]; every shard of a room gets a clone
    /// of the ONE server-wide service.
    #[must_use]
    fn with_economy(self, economy: EconomyService) -> Self;
}

impl ShardedRoomExt for ShardedRoom<DemoGame, GridPartition2<Position>> {
    fn new(index: usize, shard_count: usize, spawn_half: f32) -> Self {
        Self::with_game(
            DemoGame::new(spawn_half),
            GridPartition2::new(shard_count, spawn_half),
            index,
        )
    }

    fn with_economy(mut self, economy: EconomyService) -> Self {
        self.game_mut().set_economy(economy);
        self
    }
}

/// The demo's constructors for its sharded × spatial composite: the demo
/// shard with the kit's 2D grid AOI over the demo's wire value
/// (`ShardedSpatialRoom<DemoGame, GridPartition2<Position>, Grid2>`).
pub trait ShardedSpatialRoomExt: Sized {
    /// Build shard `index` of a `shard_count`-shard room over a square
    /// map of half-size `spawn_half`, broadcasting with cells of
    /// `cell_size` world units (see [`ShardedRoomExt::new`] for the
    /// shared halves).
    #[must_use]
    fn new(index: usize, shard_count: usize, spawn_half: f32, cell_size: f32) -> Self;

    /// Attach the economy service handle (see
    /// [`ShardedRoomExt::with_economy`]).
    #[must_use]
    fn with_economy(self, economy: EconomyService) -> Self;
}

impl ShardedSpatialRoomExt for ShardedSpatialRoom<DemoGame, GridPartition2<Position>, Grid2> {
    fn new(index: usize, shard_count: usize, spawn_half: f32, cell_size: f32) -> Self {
        Self::with_shard(
            ShardedRoomExt::new(index, shard_count, spawn_half),
            Grid2::new(cell_size),
        )
    }

    fn with_economy(mut self, economy: EconomyService) -> Self {
        self.game_mut().set_economy(economy);
        self
    }
}

#[cfg(test)]
mod tests;
