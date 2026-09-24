//! The demo's rooms: the kit's generic strategy rooms instantiated with
//! [`DemoGame`], plus the constructors every consumer (`gsb-server`'s
//! factories, the load generator, the tests) has always called. The
//! crate root's compatibility paths (`gsb_game::room::OpenRoom`, …) are
//! type aliases onto these instantiations.

use crate::demo::components::Position;
use crate::demo::economy::EconomyService;
use crate::demo::play::DemoGame;
use crate::demo::sectors::demo_map;
use crate::demo::spawn::DEFAULT_SPAWN_HALF;
use crate::kit::aoi::AoiRoom;
use crate::kit::pvs::SectorRoom;
use crate::kit::room::OpenRoom;
use crate::kit::space::{ConvexSectors2, Grid2, VisionGrid2};
use crate::kit::team::TeamRoom;

impl OpenRoom<DemoGame> {
    /// Build the open room over the default 100×100 arena (bit-identical
    /// spawn distribution to the pre-config rooms).
    pub fn new() -> Self {
        Self::with_spawn_half(DEFAULT_SPAWN_HALF)
    }

    /// Build the open room over a square spawn map of half-size `half`
    /// (entities spawn uniformly in `[-half, half]²`). The load
    /// generator's `spread` profile pairs this with its home distribution
    /// so spawn points and targets live on the same (possibly "wide")
    /// map.
    pub fn with_spawn_half(half: f32) -> Self {
        Self::with_game(DemoGame::new(half))
    }

    /// Attach the economy service handle (the RPC pattern's external-I/O
    /// half; the demo's economy service). The room delegates `ECONOMY`
    /// requests to it; the answer arrives on a later tick through the
    /// room's completion channel.
    pub fn with_economy(mut self, economy: EconomyService) -> Self {
        self.game_mut().set_economy(economy);
        self
    }
}

impl Default for OpenRoom<DemoGame> {
    fn default() -> Self {
        Self::new()
    }
}

/// The demo's AOI: the kit's 2D grid preset over the demo's truncated
/// `(x, y)` wire value.
impl AoiRoom<DemoGame, Grid2> {
    /// Build an AOI room with the given `cell_size` (world units per cell
    /// edge) over the default 100×100 spawn arena. Clamped to a sane
    /// minimum so a degenerate `0` cannot produce a single infinite cell.
    #[must_use]
    pub fn new(cell_size: f32) -> Self {
        Self::with_spawn_half(cell_size, DEFAULT_SPAWN_HALF)
    }

    /// Build an AOI room over a square spawn map of half-size `half` (see
    /// [`OpenRoom::with_spawn_half`]).
    #[must_use]
    pub fn with_spawn_half(cell_size: f32, half: f32) -> Self {
        Self::with_game(DemoGame::new(half), Grid2::new(cell_size))
    }
}

/// The demo's team fog: the kit's 2D vision preset over the demo's
/// `Position` (a uniform vision radius on the map plane).
impl TeamRoom<DemoGame, VisionGrid2<Position>> {
    /// Build a team-fog room with the given `vision_radius` (world units)
    /// over the default 100×100 spawn arena. Clamped to a sane minimum so a
    /// degenerate `0` cannot make vision "only the exact same point".
    #[must_use]
    pub fn new(vision_radius: f32) -> Self {
        Self::with_spawn_half(vision_radius, DEFAULT_SPAWN_HALF)
    }

    /// Build a team-fog room over a square spawn map of half-size `half`
    /// (see [`OpenRoom::with_spawn_half`]).
    #[must_use]
    pub fn with_spawn_half(vision_radius: f32, half: f32) -> Self {
        Self::with_game(DemoGame::new(half), VisionGrid2::new(vision_radius))
    }
}

/// The demo's PVS: its hand-authored four-sector map (`demo_map`) in the
/// kit's convex-sector preset over the demo's `Position`.
impl SectorRoom<DemoGame, ConvexSectors2<Position>> {
    /// Build a PVS room over the demo map and the default 100×100 spawn
    /// arena.
    #[must_use]
    pub fn new() -> Self {
        Self::with_spawn_half(DEFAULT_SPAWN_HALF)
    }

    /// Build a PVS room whose spawn map has half-size `half`. The demo
    /// *PVS map* stays the hand-authored 100×100 sectors; this only
    /// affects where joins place entities (a `spread`-profile run places
    /// them outside every sector — they land in the containment sector
    /// and see only themselves, which is exactly what the PVS strategy
    /// promises for off-map positions).
    #[must_use]
    pub fn with_spawn_half(half: f32) -> Self {
        Self::with_game(DemoGame::new(half), demo_map())
    }
}

impl Default for SectorRoom<DemoGame, ConvexSectors2<Position>> {
    fn default() -> Self {
        Self::new()
    }
}
