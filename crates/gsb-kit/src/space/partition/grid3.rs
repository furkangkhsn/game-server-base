//! [`GridPartition3`] — the volumetric grid preset of [`Partition`]
//! (KIT-ARCHITECTURE §7, BACKLOG A5): box regions over a cube, the
//! 6-neighbourhood (or, opted in, the 26-neighbourhood), the border
//! margin and the frame filter — [`GridPartition2`](super::GridPartition2)'s
//! rules on three axes.

use std::marker::PhantomData;

use bevy_ecs::component::Component;

use super::{Partition, checked_wire_scale};
use crate::space::Spatial;

/// The six face steps, in the order [`GridPartition2`](super::GridPartition2)
/// lists its edge neighbours and then the third axis: −x, +x, −y, +y,
/// −z, +z.
const FACES: [[i32; 3]; 6] = [
    [-1, 0, 0],
    [1, 0, 0],
    [0, -1, 0],
    [0, 1, 0],
    [0, 0, -1],
    [0, 0, 1],
];

/// The volumetric grid preset: a cube `[-half, half]³` split into a
/// `shape = [nx, ny, nz]` grid of box regions (in the [`Spatial`]
/// projection's axis order; index `(z · ny + y) · nx + x`, so a
/// one-layer grid numbers its regions like [`GridPartition2`](super::GridPartition2)),
/// the 6-neighbourhood (the regions sharing a face) — or, opted in with
/// [`Self::with_diagonals`], the 26-neighbourhood — and a border margin
/// of a quarter of the smallest region edge inside every face. Reads any
/// position component with an `f32` [`Spatial`] projection and any wire
/// value with an `i32` one, in ONE unit ([`Spatial`]'s unit contract,
/// checked in debug builds as in 2D) — or in the wire's own unit, declared
/// with [`Self::with_wire_scale`].
///
/// The shape is the game's: the kit cannot know which axis is height,
/// nor whether a world of 8 regions wants 2×2×2 or 4×1×2. A region owns
/// its lower bound on every axis and not its upper one (the upper face
/// belongs to the next region), positions are clamped into the grid
/// (every point has exactly one owner), and `nz = 1` gives exactly the
/// 2D preset's regions, neighbours, frame filter and band on the first
/// two axes (the only difference: a region also exports near the cube's
/// lower and upper face — the map's edge, where no neighbour listens).
pub struct GridPartition3<P> {
    shape: [usize; 3],
    /// Half-size of the cube (clamped to at least 1).
    half: f32,
    /// A region's edge on each axis.
    edge: [f32; 3],
    /// The border margin: a quarter of the smallest region edge.
    border: f32,
    /// Whether the regions sharing only an edge or a corner are
    /// neighbours too ([`Self::with_diagonals`]).
    diagonals: bool,
    /// Wire units per position unit ([`Self::with_wire_scale`]; 1).
    wire_scale: f32,
    _pos: PhantomData<fn() -> P>,
}

impl<P> GridPartition3<P> {
    /// A grid of `shape = [nx, ny, nz]` regions over a cube of half-size
    /// `half`. Every count must be at least 1 and their product 1..=256
    /// (one shard per region, the 2D preset's bound).
    #[must_use]
    pub fn new(shape: [usize; 3], half: f32) -> Self {
        let count = shape.iter().try_fold(1usize, |acc, &n| acc.checked_mul(n));
        assert!(
            count.is_some_and(|n| (1..=256).contains(&n)),
            "a GridPartition3 shape must have 1..=256 regions, got {shape:?}"
        );
        let half = half.max(1.0);
        let edge = shape.map(|n| 2.0 * half / n as f32);
        Self {
            shape,
            half,
            edge,
            border: edge[0].min(edge[1]).min(edge[2]) / 4.0,
            diagonals: false,
            wire_scale: 1.0,
            _pos: PhantomData,
        }
    }

    /// The 26-neighbourhood: the regions sharing only an EDGE or a
    /// CORNER with a region are its neighbours too — 2D's
    /// [`with_diagonals`](super::GridPartition2::with_diagonals) in
    /// space. A player near an edge or a corner of its region then sees
    /// the regions across it through their strips, and a crossing
    /// through an edge or a corner takes one hop instead of two or
    /// three. The cost: a region exchanges its strip with up to 26
    /// neighbours instead of 6 (each receiver keeps only the part near
    /// itself). Every shard of a room must use the same partition.
    #[must_use]
    pub fn with_diagonals(mut self) -> Self {
        self.diagonals = true;
        self
    }

    /// The wire's unit: `scale` wire units per position unit — 2D's
    /// [`with_wire_scale`](super::GridPartition2::with_wire_scale) on
    /// three axes (the default, 1, changes nothing).
    ///
    /// # Panics
    /// When `scale` is not a positive finite number.
    #[must_use]
    pub fn with_wire_scale(mut self, scale: f32) -> Self {
        self.wire_scale = checked_wire_scale(scale);
        self
    }

    /// A wire value in the position's unit.
    #[inline]
    fn unscale(&self, w: [i32; 3]) -> [f32; 3] {
        w.map(|v| v as f32 / self.wire_scale)
    }

    /// Region `idx`'s grid slot `[x, y, z]`.
    fn slot(&self, idx: usize) -> [usize; 3] {
        let [nx, ny, _] = self.shape;
        [idx % nx, (idx / nx) % ny, idx / (nx * ny)]
    }

    /// Region `idx`'s box: `[lo, hi]` on each axis.
    fn bounds(&self, idx: usize) -> [(f32, f32); 3] {
        let slot = self.slot(idx);
        std::array::from_fn(|axis| {
            let lo = -self.half + slot[axis] as f32 * self.edge[axis];
            (lo, lo + self.edge[axis])
        })
    }

    /// The region `step` away from `slot`, if the grid has one there.
    fn step(&self, slot: [usize; 3], step: [i32; 3]) -> Option<usize> {
        let mut at = [0usize; 3];
        for axis in 0..3 {
            let v = slot[axis] as i64 + i64::from(step[axis]);
            if !(0..self.shape[axis] as i64).contains(&v) {
                return None;
            }
            at[axis] = v as usize;
        }
        let [nx, ny, _] = self.shape;
        Some((at[2] * ny + at[1]) * nx + at[0])
    }
}

impl<P, W> Partition<W> for GridPartition3<P>
where
    P: Component + Spatial<Coord = f32>,
    W: Spatial<Coord = i32>,
{
    type Pos = P;

    fn shard_count(&self) -> usize {
        self.shape.iter().product()
    }

    /// The 2D preset's lookup on every axis: `floor((v + half) / edge)`,
    /// clamped into the grid.
    #[inline]
    fn region_of(&self, pos: &P) -> usize {
        let p = pos.spatial();
        let slot: [usize; 3] = std::array::from_fn(|axis| {
            let v = ((p[axis] + self.half) / self.edge[axis]).floor() as i32;
            v.clamp(0, self.shape[axis] as i32 - 1) as usize
        });
        let [nx, ny, _] = self.shape;
        (slot[2] * ny + slot[1]) * nx + slot[0]
    }

    /// The faces (−x, +x, −y, +y, −z, +z) — then, with
    /// [`Self::with_diagonals`], the twelve edges and the eight corners,
    /// each group in a fixed order (third axis outermost): a route's
    /// first hop prefers a face over an edge and an edge over a corner
    /// when both are shortest (as 2D prefers an edge over a corner).
    fn neighbors(&self, idx: usize) -> Vec<usize> {
        let slot = self.slot(idx);
        let mut neighbors = Vec::with_capacity(if self.diagonals { 26 } else { 6 });
        neighbors.extend(FACES.iter().filter_map(|&f| self.step(slot, f)));
        if self.diagonals {
            for axes in [2, 3] {
                for i in 0..27i32 {
                    let step = [i % 3 - 1, (i / 3) % 3 - 1, i / 9 - 1];
                    if step.iter().filter(|&&d| d != 0).count() == axes {
                        neighbors.extend(self.step(slot, step));
                    }
                }
            }
        }
        neighbors
    }

    /// Within `border` of any face of the region box (the export covers
    /// the whole border shell; each receiver's [`Self::admits`] keeps
    /// the part near itself).
    #[inline]
    fn exports(&self, idx: usize, pos: &P) -> bool {
        let p = pos.spatial();
        let b = self.border;
        let bounds = self.bounds(idx);
        (0..3).any(|axis| {
            let (lo, hi) = bounds[axis];
            p[axis] - lo < b || hi - p[axis] < b
        })
    }

    /// Within `border` of the region box, including the thin overlap
    /// into it — the wire read at its scale.
    #[inline]
    fn admits(&self, idx: usize, wire: &W) -> bool {
        let w = self.unscale(wire.spatial());
        let b = self.border;
        let bounds = self.bounds(idx);
        (0..3).all(|axis| {
            let (lo, hi) = bounds[axis];
            w[axis] >= lo - b && w[axis] <= hi + b
        })
    }

    /// In the box, or less than `margin` outside it on the worst axis —
    /// `margin` clamped to the border margin (the trait docs).
    fn holds(&self, idx: usize, pos: &P, margin: f32) -> bool {
        let p = pos.spatial();
        let bounds = self.bounds(idx);
        let outside = (0..3)
            .map(|axis| (bounds[axis].0 - p[axis]).max(p[axis] - bounds[axis].1))
            .fold(f32::NEG_INFINITY, f32::max);
        <Self as Partition<W>>::region_of(self, pos) == idx || outside < margin.min(self.border)
    }

    /// The unit contract: the wire's projection, at the wire scale, lies
    /// within one border margin of the position's on every axis (2D's).
    fn debug_check_wire(&self, pos: &P, wire: &W) {
        if cfg!(debug_assertions) {
            let p = pos.spatial();
            let w = wire.spatial();
            let at = self.unscale(w);
            let (b, s) = (self.border, self.wire_scale);
            assert!(
                (0..3).all(|axis| (at[axis] - p[axis]).abs() <= b),
                "GridPartition3: an entity at {p:?} has the wire projection {w:?}, \
                 {at:?} at the wire scale {s}, more than the border margin {b} \
                 away — the wire type's Spatial must report the position's unit, \
                 or the partition declare the wire's (with_wire_scale; admits \
                 compares it with the region boxes)"
            );
        }
    }
}
