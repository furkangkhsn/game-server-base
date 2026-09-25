//! The fixture's PVS map and the constructors the tests build the rooms
//! over the fixture with.

use crate::aoi::AoiRoom;
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::sharded::{ShardedRoom, ShardedSpatialRoom, ShardedTeamRoom};
use crate::space::{ConvexSectors2, Grid2, GridPartition2, Sector, VisionGrid2};
use crate::team::TeamRoom;

use super::{Fixture, Position, WirePos};

// ── The PVS map: four convex sectors tiling [-50, 50]² ────────────────
//
//   C (NW) | D (NE)      y ∈ [20, 50], split at x = -10
//   -------+-------
//   A (W)  | B (E)       y ∈ [-50, 20], split at x = 0 (a wall)
//
// A sees A, C; B sees B, D; C sees A, C, D; D sees B, C, D. A and B are
// adjacent but do not see each other.

pub(crate) const SECTOR_WEST: u8 = 0;
pub(crate) const SECTOR_EAST: u8 = 1;
pub(crate) const SECTOR_NW: u8 = 2;
const SECTOR_NE: u8 = 3;
/// The preset's containment sector (one past the last polygon).
pub(crate) const SECTOR_OUT: u8 = 4;

/// The fixture map in the kit's convex-sector preset.
pub(crate) fn fixture_map() -> ConvexSectors2<Position> {
    let polygons = [
        [(-50.0, -50.0), (0.0, -50.0), (0.0, 20.0), (-50.0, 20.0)],
        [(0.0, -50.0), (50.0, -50.0), (50.0, 20.0), (0.0, 20.0)],
        [(-50.0, 20.0), (-10.0, 20.0), (-10.0, 50.0), (-50.0, 50.0)],
        [(-10.0, 20.0), (50.0, 20.0), (50.0, 50.0), (-10.0, 50.0)],
    ];
    let visible: [&[u8]; 4] = [
        &[SECTOR_WEST, SECTOR_NW],
        &[SECTOR_EAST, SECTOR_NE],
        &[SECTOR_WEST, SECTOR_NW, SECTOR_NE],
        &[SECTOR_EAST, SECTOR_NW, SECTOR_NE],
    ];
    ConvexSectors2::new(
        polygons.iter().map(|p| p.to_vec()).collect(),
        visible
            .iter()
            .map(|list| list.iter().map(|&s| Sector(s)).collect())
            .collect(),
    )
}

// ── Constructors: the rooms over the fixture, as the tests build them ─

impl OpenRoom<Fixture> {
    pub(crate) fn new() -> Self {
        Self::with_game(Fixture::default())
    }
}

impl AoiRoom<Fixture, Grid2> {
    pub(crate) fn new(cell_size: f32) -> Self {
        Self::with_game(Fixture::default(), Grid2::new(cell_size))
    }
}

impl TeamRoom<Fixture, VisionGrid2<Position>> {
    pub(crate) fn new(vision_radius: f32) -> Self {
        Self::with_game(Fixture::default(), VisionGrid2::new(vision_radius))
    }
}

impl SectorRoom<Fixture, ConvexSectors2<Position>> {
    pub(crate) fn new() -> Self {
        Self::with_game(Fixture::default(), fixture_map())
    }
}

impl ShardedRoom<Fixture, GridPartition2<Position>> {
    /// Shard `index` of `shard_count` over `[-half, half]²`.
    pub(crate) fn new(index: usize, shard_count: usize, half: f32) -> Self {
        Self::with_game(
            Fixture::default(),
            GridPartition2::new(shard_count, half),
            index,
        )
    }
}

impl ShardedSpatialRoom<Fixture, GridPartition2<Position>, Grid2> {
    pub(crate) fn new(index: usize, shard_count: usize, half: f32, cell_size: f32) -> Self {
        Self::with_shard(
            ShardedRoom::<Fixture, GridPartition2<Position>>::new(index, shard_count, half),
            Grid2::new(cell_size),
        )
    }
}

/// Where a lent fixture record stands for vision: its (truncated) wire
/// position.
pub(crate) fn fix_lent_pos(w: &WirePos) -> Option<Position> {
    Some(Position {
        x: w.x as f32,
        y: w.y as f32,
    })
}

impl ShardedTeamRoom<Fixture, GridPartition2<Position>, VisionGrid2<Position>> {
    /// Shard `index` of `shard_count` over `[-half, half]²`, team vision
    /// of `radius`.
    pub(crate) fn new(index: usize, shard_count: usize, half: f32, radius: f32) -> Self {
        Self::with_shard(
            ShardedRoom::<Fixture, GridPartition2<Position>>::new(index, shard_count, half),
            VisionGrid2::new(radius),
            fix_lent_pos,
        )
    }
}
