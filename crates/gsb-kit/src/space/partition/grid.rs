//! [`GridPartition2`] — the 2D grid preset of [`Partition`] (§7): the
//! region rectangles, the 4- or 8-neighbourhood, the border margin and
//! the frame filter.

use std::marker::PhantomData;

use bevy_ecs::component::Component;

use super::{Partition, grid_shape, region_at};
use crate::space::Planar;

/// The 2D grid preset: a square map `[-half, half]²` on the ground plane
/// split into a `grid_shape(shard_count)` grid of rectangular regions,
/// the 4-neighbourhood (west, east, north, south) — or, opted in with
/// [`Self::with_diagonals`], the 8-neighbourhood — and a border margin of
/// a quarter of the smaller region edge on each side of every shared
/// edge. Reads any position component with an `f32` [`Planar`]
/// projection and any wire value with an `i32` one.
///
/// **One unit for both.** [`Partition::admits`] compares the WIRE
/// value's projection with the region rectangles, which are in the
/// POSITION's unit (`half` is). So the wire type's [`Planar`] must report
/// the position's unit: a game that quantizes its wire finer than its
/// position (centimetres, decimetres over metres) projects the wire back
/// to the position's unit — integer division to whole units is enough,
/// the margin being a quarter region wide. Otherwise the plain
/// [`crate::sharded::ShardedRoom`]'s frame filter silently keeps the
/// wrong part of every neighbour's strip (a 128 m margin reads as 12.8 m
/// at decimetres). Debug builds catch it: [`Partition::debug_check_wire`]
/// panics when an exported entity's wire projection lies more than one
/// border margin from its position's.
pub struct GridPartition2<P> {
    shard_count: usize,
    rows: usize,
    cols: usize,
    /// Half-size of the square map (clamped to at least 1).
    half: f32,
    cell_w: f32,
    cell_h: f32,
    /// The border margin: a quarter of the smaller region edge.
    border: f32,
    /// Whether the regions sharing only a corner are neighbours too (the
    /// 8-neighbourhood, [`Self::with_diagonals`]).
    diagonals: bool,
    _pos: PhantomData<fn() -> P>,
}

impl<P> GridPartition2<P> {
    /// A grid of `shard_count` regions (1..=256) over a map of half-size
    /// `half`.
    #[must_use]
    pub fn new(shard_count: usize, half: f32) -> Self {
        let (rows, cols) = grid_shape(shard_count);
        let half = half.max(1.0);
        let cell_w = 2.0 * half / cols as f32;
        let cell_h = 2.0 * half / rows as f32;
        Self {
            shard_count,
            rows,
            cols,
            half,
            cell_w,
            cell_h,
            border: cell_w.min(cell_h) / 4.0,
            diagonals: false,
            _pos: PhantomData,
        }
    }

    /// The 8-neighbourhood: the regions that share only a CORNER with a
    /// region are its neighbours too. A player near a corner then sees
    /// the diagonal region's corner through the border strip (with the
    /// 4-neighbourhood nothing is lent across a corner), and a crossing
    /// through a corner — or a jump — into the diagonal region takes one
    /// hop instead of two. The cost: every region exchanges its strip
    /// with up to eight neighbours instead of four (each receiver's
    /// [`Partition::admits`] keeps only the part near itself — for a
    /// diagonal neighbour, the corner square). Every shard of a room must
    /// use the same partition. Not the default: the 4-neighbourhood is
    /// the preset's established topology (routes, exchange fan-out).
    #[must_use]
    pub fn with_diagonals(mut self) -> Self {
        self.diagonals = true;
        self
    }

    /// Region `idx`'s rectangle `[x0, x1] × [y0, y1]`.
    fn rect(&self, idx: usize) -> (f32, f32, f32, f32) {
        let row = idx / self.cols;
        let col = idx % self.cols;
        let x0 = -self.half + col as f32 * self.cell_w;
        let y0 = -self.half + row as f32 * self.cell_h;
        (x0, x0 + self.cell_w, y0, y0 + self.cell_h)
    }
}

impl<P, W> Partition<W> for GridPartition2<P>
where
    P: Component + Planar<Coord = f32>,
    W: Planar<Coord = i32>,
{
    type Pos = P;

    fn shard_count(&self) -> usize {
        self.shard_count
    }

    #[inline]
    fn region_of(&self, pos: &P) -> usize {
        let [x, y] = pos.planar();
        region_at(
            x,
            y,
            self.half,
            (self.rows, self.cols),
            (self.cell_w, self.cell_h),
        )
    }

    /// West, east, north, south — then, with [`Self::with_diagonals`],
    /// the corners (north-west, north-east, south-west, south-east): the
    /// edge neighbours stay first, so a route's first hop prefers an edge
    /// when both are shortest.
    fn neighbors(&self, idx: usize) -> Vec<usize> {
        let row = idx / self.cols;
        let col = idx % self.cols;
        let mut neighbors = Vec::with_capacity(if self.diagonals { 8 } else { 4 });
        let (west, east) = (col > 0, col + 1 < self.cols);
        let (north, south) = (row > 0, row + 1 < self.rows);
        if west {
            neighbors.push(idx - 1);
        }
        if east {
            neighbors.push(idx + 1);
        }
        if north {
            neighbors.push(idx - self.cols);
        }
        if south {
            neighbors.push(idx + self.cols);
        }
        if self.diagonals {
            if north && west {
                neighbors.push(idx - self.cols - 1);
            }
            if north && east {
                neighbors.push(idx - self.cols + 1);
            }
            if south && west {
                neighbors.push(idx + self.cols - 1);
            }
            if south && east {
                neighbors.push(idx + self.cols + 1);
            }
        }
        neighbors
    }

    /// Within `border` of any edge of the region rectangle (the export
    /// covers the whole border; each receiver's [`Self::admits`] keeps
    /// the part near itself).
    #[inline]
    fn exports(&self, idx: usize, pos: &P) -> bool {
        let [x, y] = pos.planar();
        let (x0, x1, y0, y1) = self.rect(idx);
        let b = self.border;
        (x - x0) < b || (x1 - x) < b || (y - y0) < b || (y1 - y) < b
    }

    /// Within `border` of the region rectangle, including the thin
    /// overlap into it.
    #[inline]
    fn admits(&self, idx: usize, wire: &W) -> bool {
        let [x, y] = wire.planar();
        let (x0, x1, y0, y1) = self.rect(idx);
        let b = self.border;
        x as f32 >= x0 - b && x as f32 <= x1 + b && y as f32 >= y0 - b && y as f32 <= y1 + b
    }

    /// The unit contract (type docs): the wire's projection lies within
    /// one border margin of the position's on both axes — a quantization
    /// in the position's unit is off by at most a unit or so, a finer
    /// unit is off by a factor, which exceeds the margin for any entity
    /// farther than a fraction of a margin from the origin.
    fn debug_check_wire(&self, pos: &P, wire: &W) {
        if cfg!(debug_assertions) {
            let [px, py] = pos.planar();
            let [wx, wy] = wire.planar();
            let b = self.border;
            assert!(
                (wx as f32 - px).abs() <= b && (wy as f32 - py).abs() <= b,
                "GridPartition2: an entity at ({px}, {py}) has the wire projection \
                 ({wx}, {wy}), more than the border margin {b} away — the wire \
                 type's Planar must report the position's unit (admits compares \
                 it with the region rectangles)"
            );
        }
    }
}
