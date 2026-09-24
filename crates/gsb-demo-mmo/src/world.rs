//! The MMO's world geometry — the instance data of its two kit presets:
//! the shard grid ([`GridPartition2`]) and the AOI grid ([`Grid2`]), both
//! on the GROUND PLANE of the 3D data (through `Planar` = `[x, z]`).
//!
//! **Parameters and why.**
//! - The map is `[-WORLD_HALF, WORLD_HALF]²` = 1 024 m × 1 024 m on the
//!   ground, [`CEILING`] = 200 m of air above it (flyers).
//! - [`SHARDS`] = 4: a 2×2 shard grid (`grid_shape(4)`) of 512 m regions.
//!   Row-major: shard 0 = (x < 0, z < 0), 1 = (x ≥ 0, z < 0), 2 = (x < 0,
//!   z ≥ 0), 3 = (x ≥ 0, z ≥ 0). The preset's border margin is a quarter
//!   of the region edge: 128 m.
//! - [`CELL_SIZE`] = 64 m AOI cells, a 3×3 view: a player sees 64–128 m
//!   around it on the ground. The seams at x = 0 and z = 0 fall on cell
//!   edges, and a cell's view reaches at most ONE cell (64 m) across a
//!   seam — inside the 128 m border strip, so everything a player at the
//!   seam should see from the neighbouring shard is lent to its shard —
//!   across a region CORNER too: the grid is the preset's 8-neighbourhood
//!   ([`GridPartition2::with_diagonals`]), a world with corners being the
//!   MMO's reality (with the default 4-neighbourhood the diagonal shard
//!   lent nothing — `docs/KIT-ARCHITECTURE.md` §10, F2). A jump into the
//!   diagonal shard (a waystone `Travel`) is one hop.

use gsb_kit::space::{Grid2, GridPartition2, shard_at};

use crate::components::Pos3;

/// Half the map's edge on the ground (metres).
pub const WORLD_HALF: f32 = 512.0;

/// The height of the sky (metres): flyers live in `(0, CEILING]`.
pub const CEILING: f32 = 200.0;

/// The number of shards (a 2×2 grid).
pub const SHARDS: usize = 4;

/// The AOI cell edge on the ground (metres, as seen through the wire's
/// `Planar` projection — see `codec.rs`).
pub const CELL_SIZE: f32 = 64.0;

/// The same cell edge in the wire's unit (decimetres) — what a client
/// divides its records' coordinates by ([`client_cell`]).
pub const CELL_DM: i32 = 640;

/// A player's run speed (m/s).
pub const RUN_SPEED: f32 = 7.0;

/// A player's hit points.
pub const PLAYER_HP: u16 = 100;

/// Melee reach of an attack (3D metres: a flyer high above is out of
/// reach).
pub const ATTACK_RANGE: f32 = 30.0;

/// Damage per landed attack.
pub const ATTACK_DAMAGE: u16 = 25;

/// How long a player stays in combat after its last landed attack, in
/// ticks: 6 s at the MMO's 30 Hz — the usual MMO "in combat" timer. A
/// disconnected character is not logged out before it runs out.
pub const COMBAT_TICKS: u64 = 180;

/// The waystones (`Travel` destinations) — one at the centre of every
/// region, on the ground. Waystone `i` is in shard `i`'s region.
pub const WAYSTONES: [[f32; 2]; SHARDS] = [
    [-256.0, -256.0],
    [256.0, -256.0],
    [-256.0, 256.0],
    [256.0, 256.0],
];

/// The shard grid: [`SHARDS`] regions over the map, every region a
/// neighbour of the other three (the kit's preset with its
/// 8-neighbourhood, reading [`Pos3`] and the wire value through
/// `Planar`).
#[must_use]
pub fn partition() -> GridPartition2<Pos3> {
    GridPartition2::new(SHARDS, WORLD_HALF).with_diagonals()
}

/// The AOI grid: [`CELL_SIZE`] cells, 3×3 view (the kit's preset).
#[must_use]
pub fn aoi_grid() -> Grid2 {
    Grid2::new(CELL_SIZE)
}

/// The shard whose region owns `pos` (height ignored) — the join router's
/// answer for a character logging in at `pos` (the server's `home_shard`).
#[must_use]
pub fn home_shard(pos: &Pos3) -> usize {
    shard_at(pos.x, pos.z, WORLD_HALF, SHARDS)
}

/// The waystone nearest to `pos` on the ground (where the logout bot
/// walks a disconnected character).
#[must_use]
pub fn nearest_waystone(pos: &Pos3) -> [f32; 2] {
    let d2 = |w: &[f32; 2]| (w[0] - pos.x).powi(2) + (w[1] - pos.z).powi(2);
    WAYSTONES
        .into_iter()
        .min_by(|a, b| d2(a).total_cmp(&d2(b)))
        .expect("the waystone list is not empty")
}

/// The client's AOI cell of a record at `(x, z)` DECIMETRES:
/// `(floor(x / 640), floor(z / 640))` — exactly the kit's `Grid2` cell of
/// the wire value (pinned by a unit test), computed without the kit.
#[must_use]
pub fn client_cell(x_dm: i32, z_dm: i32) -> (i32, i32) {
    (x_dm.div_euclid(CELL_DM), z_dm.div_euclid(CELL_DM))
}
