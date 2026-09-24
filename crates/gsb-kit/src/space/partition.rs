//! [`Partition`] — the sharding seam over instance data
//! (KIT-ARCHITECTURE §4.2), and [`GridPartition2`], its 2D grid preset
//! (§7).
//!
//! A partition is not a property of the position type: it carries
//! INSTANCE data (map size, shard count, border width) and reads two
//! kinds of value — the simulation position (which region owns an
//! entity, whether it is near a border) and the wire value (whether a
//! neighbour's border record is near this shard).

use bevy_ecs::component::Component;

mod grid;

pub use grid::GridPartition2;

/// How a map is split into shard regions, which shards border each
/// other, and what crosses a border. `W` is the game's wire value (the
/// border strip's payload — `Strip = Wire`).
pub trait Partition<W>: Send + 'static {
    /// The simulation position component regions are looked up by.
    type Pos: Component;

    /// The number of regions (one shard each).
    fn shard_count(&self) -> usize;

    /// The region owning `pos` — exactly one for every position (a stray
    /// coordinate must still have an owner).
    fn region_of(&self, pos: &Self::Pos) -> usize;

    /// The regions bordering region `idx`, in a stable order: the shards
    /// `idx` exchanges border records and migrations with. The relation
    /// must be symmetric and the graph connected (an entity reaches any
    /// region hop by hop).
    fn neighbors(&self, idx: usize) -> Vec<usize>;

    /// Sender side: whether an entity of region `idx` at `pos` is near
    /// enough to the region's edge to be exported to the neighbours.
    fn exports(&self, idx: usize, pos: &Self::Pos) -> bool;

    /// Receiver side: whether a neighbour's border record with wire value
    /// `wire` is near enough to region `idx` to be visible there (the
    /// frame filter — a neighbour exports its whole border, and the parts
    /// far from `idx` are not).
    fn admits(&self, idx: usize, wire: &W) -> bool;

    /// Debug builds: check that `wire` — the codec's wire value of the
    /// entity at `pos` — agrees with `pos` the way [`Self::admits`]
    /// assumes, and panic with the reason when it does not (the default
    /// checks nothing). The sharded rooms call it for every entity they
    /// export when they rebuild the border strip; an implementation
    /// keeps its test inside `cfg!(debug_assertions)`, so a release build
    /// pays nothing. [`GridPartition2`] checks the unit contract of
    /// [`Planar`](crate::space::Planar): the wire's projection must be
    /// in the position's unit.
    fn debug_check_wire(&self, _pos: &Self::Pos, _wire: &W) {}
}

/// The grid shape for `shard_count` shards: `rows` = the largest divisor
/// of `shard_count` that is ≤ √N, `cols` = N / rows — the shape closest
/// to a square (balanced region sizes). `shard_count` must be 1..=256.
///
/// Examples: 1→1×1, 2→1×2, 4→2×2, 6→2×3, 8→2×4, 12→3×4, 16→4×4,
/// 25→5×5.
pub fn grid_shape(shard_count: usize) -> (usize, usize) {
    assert!(
        (1..=256).contains(&shard_count),
        "shard_count must be 1..=256 (grid topology), got {shard_count}"
    );
    let sqrt = (shard_count as f64).sqrt().floor() as usize;
    for d in (1..=sqrt).rev() {
        if shard_count.is_multiple_of(d) {
            return (d, shard_count / d);
        }
    }
    (1, shard_count) // unreachable: d=1 always divides
}

/// The shard index owning the plane point `(x, y)` on a map of half-size
/// `half`, partitioned into `shard_count` shards in a `grid_shape` grid —
/// [`GridPartition2`]'s region lookup as a free function (a join router
/// that has only a spawn point, no component, uses it). The map spans
/// `[-half, half]²`; each column spans `2*half/cols` in x, each row
/// `2*half/rows` in y. Positions are clamped into the grid (the map has
/// no walls, but a stray coordinate must still own exactly one shard —
/// the "exactly one owner" invariant).
pub fn shard_at(x: f32, y: f32, half: f32, shard_count: usize) -> usize {
    let (rows, cols) = grid_shape(shard_count);
    let cell_w = 2.0 * half / cols as f32;
    let cell_h = 2.0 * half / rows as f32;
    region_at(x, y, half, (rows, cols), (cell_w, cell_h))
}

/// The row-major region of `(x, y)` in a grid of `rows × cols` cells of
/// `cell_w × cell_h` over `[-half, half]²`, clamped into the grid.
#[inline]
fn region_at(
    x: f32,
    y: f32,
    half: f32,
    (rows, cols): (usize, usize),
    (cell_w, cell_h): (f32, f32),
) -> usize {
    let col = (((x + half) / cell_w).floor() as i32).clamp(0, (cols - 1) as i32);
    let row = (((y + half) / cell_h).floor() as i32).clamp(0, (rows - 1) as i32);
    row as usize * cols + col as usize
}
