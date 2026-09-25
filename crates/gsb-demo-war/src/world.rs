//! The war map — the instance data of the game's two kit presets: the
//! shard grid ([`GridPartition2`]) and the vision model
//! ([`VisionGrid2`]), both on the GROUND PLANE of the 3D data (through
//! `Planar` = `[x, z]`) — and where everything stands on it.
//!
//! **The map.** `[-WORLD_HALF, WORLD_HALF]²` = 1 600 m × 1 600 m, a 2×2
//! shard grid of 800 m regions (row-major: shard 0 = (x < 0, z < 0), 1 =
//! (x ≥ 0, z < 0), 2 = (x < 0, z ≥ 0), 3 = (x ≥ 0, z ≥ 0)), the preset's
//! border strip a quarter of the region edge: 200 m. Faction `f`'s base
//! lies deep in region `f` ([`BASES`]); region 3 has no base — the
//! contested ground, with one capture point in its middle and one right
//! by the map's centre, where the four regions meet ([`POINTS`]).
//!
//! **Towers.** Every faction keeps a watchtower in EVERY region
//! ([`tower`]): a faction has vision on each shard, whether its players
//! are there or not — the scenario the `team × sharded` composite exists
//! for (`docs/CROSS-SHARD.md` §8b: a tower on shard 0 spots an enemy
//! there, and its faction's players on shard 3 see that enemy). The
//! three towers of a region sit 120° apart on a 180 m ring round the
//! region's centre: 312 m from each other, out of each other's sight,
//! more than 220 m from every seam (outside the border strip) and more
//! than 110 m from every base.
//!
//! **Vision.** One radius for every unit, towers included
//! ([`VISION_RADIUS`], the kit's uniform-radius preset). A per-unit
//! radius (a tower seeing further) is not in the kit's `Vision` seam
//! (BACKLOG A8): the game would write its own `Vision` whose position
//! type carries the radius. Cephe does not — see KIT-ARCHITECTURE §10
//! "W2 sonucu".

use gsb_kit::space::{GridPartition2, VisionGrid2, shard_at};
use gsb_kit::team::Team;

use crate::codec::WarWire;
use crate::components::Pos3;

/// Half the map's edge on the ground (metres). 8 000 dm: every ground
/// coordinate on the wire stays a 2-byte zig-zag varint (≤ 8 191).
pub const WORLD_HALF: f32 = 800.0;

/// The height of the sky (metres).
pub const CEILING: f32 = 100.0;

/// The number of shards (a 2×2 grid).
pub const SHARDS: usize = 4;

/// The number of factions.
pub const FACTIONS: u8 = 3;

/// Every unit's vision radius on the ground (metres).
pub const VISION_RADIUS: f32 = 60.0;

/// A player's run speed (m/s).
pub const RUN_SPEED: f32 = 7.0;

/// A player's hit points.
pub const PLAYER_HP: u16 = 100;

/// Melee reach of an attack, on the ground (metres).
pub const ATTACK_RANGE: f32 = 20.0;

/// Damage per landed attack: four blows fell a player.
pub const ATTACK_DAMAGE: u16 = 25;

/// A tower's platform height (metres) — on the wire only: vision is on
/// the ground plane.
pub const TOWER_HEIGHT: f32 = 12.0;

/// How close to a capture point a player must stand to take it
/// (metres, ground). Smaller than the distance from either point to the
/// nearest seam: a capture is decided on one shard.
pub const CAPTURE_RADIUS: f32 = 15.0;

/// Ticks in a row one faction must hold a point alone to take it (3 s
/// at 30 Hz).
pub const CAPTURE_TICKS: u32 = 90;

/// Faction `f`'s base (ground `(x, z)`), deep in region `f`: where an
/// unsaved player of the faction spawns and a fallen one respawns.
pub const BASES: [[f32; 2]; FACTIONS as usize] =
    [[-560.0, -560.0], [560.0, -560.0], [-560.0, 560.0]];

/// The capture points (ground `(x, z)`), both in region 3: the MIDDLE,
/// 85 m from the corner where the four regions meet, and the centre of
/// region 3.
pub const POINTS: [[f32; 2]; 2] = [[60.0, 60.0], [400.0, 400.0]];

/// The centres of the four regions.
const REGION_CENTRES: [[f32; 2]; SHARDS] = [
    [-400.0, -400.0],
    [400.0, -400.0],
    [-400.0, 400.0],
    [400.0, 400.0],
];

/// Where a faction's tower stands relative to its region's centre: on a
/// 180 m ring, faction 0 at 45° (from +x toward +z), the others 120° on
/// — no tower stands within 110 m of a base (a base lies 226 m from its
/// region's centre at 225°, 315° or 135°).
const TOWER_RING: [[f32; 2]; FACTIONS as usize] = [
    [127.279_22, 127.279_22],
    [-173.866_82, 46.587_15],
    [46.587_15, -173.866_82],
];

/// Faction `faction`'s tower in region `region` (ground `(x, z)`).
#[must_use]
pub fn tower(faction: Team, region: usize) -> [f32; 2] {
    let [cx, cz] = REGION_CENTRES[region % SHARDS];
    let [ox, oz] = TOWER_RING[usize::from(faction.0) % TOWER_RING.len()];
    [cx + ox, cz + oz]
}

/// The shard grid: [`SHARDS`] regions over the map, every region a
/// neighbour of the other three (the preset's 8-neighbourhood: the
/// middle point sits by the corner where all four meet).
#[must_use]
pub fn partition() -> GridPartition2<Pos3> {
    GridPartition2::new(SHARDS, WORLD_HALF).with_diagonals()
}

/// The vision model: [`VISION_RADIUS`] on the ground for every unit.
#[must_use]
pub fn vision() -> VisionGrid2<Pos3> {
    VisionGrid2::new(VISION_RADIUS)
}

/// The shard whose region owns `pos` (height ignored) — the join
/// router's answer for a unit at `pos`.
#[must_use]
pub fn home_shard(pos: &Pos3) -> usize {
    shard_at(pos.x, pos.z, WORLD_HALF, SHARDS)
}

/// Where a LENT record (a neighbour's border strip) stands for vision:
/// its decimetre position, back in metres. The kit's
/// `ShardedTeamRoom::with_shard` takes it; a lent unit is a vision
/// TARGET only (its own shard computes what it sees).
#[must_use]
pub fn lent_pos(w: &WarWire) -> Option<Pos3> {
    Some(w.pos())
}

/// The base of `faction` as a position on the ground.
#[must_use]
pub fn base(faction: Team) -> Pos3 {
    let [x, z] = BASES[usize::from(faction.0) % BASES.len()];
    Pos3::ground(x, z)
}

#[cfg(test)]
mod tests;
