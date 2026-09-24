//! The MMO's [`RecordCodec`] (KIT-ARCHITECTURE §4.1): every entity with
//! a [`Pos3`] is broadcast; its record is the position QUANTIZED to
//! integer decimetres plus its [`Vitals`] — [`MmoWire`], written as
//! `mmo.proto`'s `EntityRecord { entity = 1; x, y, z = 2..4; kind = 5;
//! hp = 6 }`. `Dirty` is position OR vitals changed (a mob losing hit
//! points is news although it did not move).
//!
//! **Why decimetres (`i32`, rounded to nearest).**
//! - *Size:* the map spans ±5 120 dm on the ground and 0..2 000 dm in
//!   height; a zig-zag varint stays at 2 bytes up to |v| = 8 191 — every
//!   coordinate anywhere in the world is at most 2 bytes: the three
//!   coordinates take at most 9 bytes of the record (tags included),
//!   against 15 for three `float` fields.
//!   Centimetres push every ground coordinate beyond ±81.9 m to 3 bytes:
//!   an MMO's record count is its bandwidth, and most records are far
//!   from the origin.
//! - *Precision:* 10 cm is below what an MMO client renders at nameplate
//!   distance, and clients interpolate between snapshots anyway.
//! - *A change threshold for free:* the wire value is the delta engine's
//!   change test. A player running 7 m/s moves 23 cm per 30 Hz tick —
//!   a new record every tick while it runs; a mob wandering at 1 m/s
//!   moves 3.3 cm per tick — a new record every third tick. Idle mobs
//!   and players cost nothing.
//! - *Rounding, not truncation:* truncation makes the cell around 0
//!   twice as wide (−0.09 m and +0.09 m both become 0) — right on the
//!   MMO's shard seams at x = 0 and z = 0.
//!
//! **The wire's `Planar` is in METRES** (`floor(dm / 10)`), the
//! position's unit, not in decimetres: then every ground-plane preset
//! works in the one world unit — `Grid2`'s cell edge is in metres, and
//! `GridPartition2::admits` (the sharded rooms' border frame filter —
//! the MMO's spatial composite applies it too) compares the
//! wire projection with a region rectangle in the POSITION's unit —
//! `Planar`'s unit contract (Phase 4 found it unwritten, finding F3 in
//! `docs/KIT-ARCHITECTURE.md` §10; the kit now documents it and a debug
//! build checks it on every exported entity): a `[x_dm, z_dm]`
//! projection compiles and misfilters there. Coarsening to
//! whole metres costs nothing: a cell edge (64 m) is a whole number of
//! metres, so the cell is still `floor(x_dm / 640)`
//! ([`crate::world::client_cell`]).

use bevy_ecs::prelude::{Changed, Or};
use bytes::BytesMut;
use gsb_kit::codec::RecordCodec;
use gsb_kit::space::Planar;
use prost::Message;

use crate::components::{Kind, Pos3, Vitals};
use crate::mmo::EntityRecord;

/// One entity's quantized record — the MMO's wire value (and the border
/// strip's payload: `Strip = Wire`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MmoWire {
    /// Position, decimetres.
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub kind: Kind,
    pub hp: u16,
}

/// Metres → decimetres, rounded to nearest (saturating outside `i32`).
#[inline]
#[must_use]
pub fn to_dm(metres: f32) -> i32 {
    (metres * 10.0).round() as i32
}

/// Decimetres → metres.
#[inline]
#[must_use]
pub fn from_dm(dm: i32) -> f32 {
    dm as f32 / 10.0
}

/// The ground-plane projection of the wire value, in whole METRES (module
/// docs: the position's unit, which `GridPartition2::admits` assumes).
impl Planar for MmoWire {
    type Coord = i32;

    #[inline]
    fn planar(&self) -> [i32; 2] {
        [self.x.div_euclid(10), self.z.div_euclid(10)]
    }
}

impl MmoWire {
    /// The quantized record of an entity at `pos` with `vitals`.
    #[must_use]
    pub fn of(pos: &Pos3, vitals: &Vitals) -> Self {
        Self {
            x: to_dm(pos.x),
            y: to_dm(pos.y),
            z: to_dm(pos.z),
            kind: vitals.kind,
            hp: vitals.hp,
        }
    }
}

/// The MMO's record codec — zero-sized (the quantization is fixed).
#[derive(Debug, Clone, Copy, Default)]
pub struct MmoCodec;

impl RecordCodec for MmoCodec {
    type Marker = Pos3;
    type Query = (&'static Pos3, &'static Vitals);
    type Dirty = Or<(Changed<Pos3>, Changed<Vitals>)>;
    type Wire = MmoWire;

    #[inline]
    fn wire(&self, (pos, vitals): (&Pos3, &Vitals)) -> MmoWire {
        MmoWire::of(pos, vitals)
    }

    #[inline]
    fn encode(&self, id: u64, w: &MmoWire, out: &mut BytesMut) {
        EntityRecord {
            entity: id,
            x: w.x,
            y: w.y,
            z: w.z,
            kind: w.kind as i32,
            hp: u32::from(w.hp),
        }
        .encode(out)
        .expect("protobuf encode into an in-memory buffer failed");
    }
}

#[cfg(test)]
mod tests;
