//! The arena's [`RecordCodec`] (KIT-ARCHITECTURE §4.1): every unit with
//! a [`Pos3`] is broadcast; its record is the position QUANTIZED to
//! integer centimetres, [`Cm3`], written as `arena.proto`'s
//! `UnitRecord { uint64 entity = 1; sint32 x = 2; sint32 y = 3;
//! sint32 z = 4; }`.
//!
//! **Why centimetres (`i32`, rounded to nearest).**
//! - *Precision:* 1 cm is below anything a client renders or aims with
//!   at arena scale; a coarser step (decimetres) would visibly stair-step
//!   a unit climbing a ramp at 12 m/s interpolated over 33 ms ticks
//!   (≈ 40 cm per tick).
//! - *Size:* the arena spans ±5 000 cm horizontally and 0..3 000 cm in
//!   height, and a zig-zag varint stays at 2 bytes up to |v| = 8 191 —
//!   so every coordinate anywhere in the arena is at most 2 bytes and a
//!   record at most 11 bytes (1-byte id), against 17 for three `float`
//!   fields. Millimetres would push most coordinates to 3 bytes for
//!   precision nobody sees; decimetres would save nothing below 6.3 m.
//! - *Range:* `i32` centimetres cover ±21 474 km — the quantization can
//!   never overflow inside the arena (and `as` saturates outside it).
//! - *Rounding, not truncation:* truncation toward zero makes the cell
//!   around 0 twice as wide as every other (−0.9 cm and +0.9 cm both
//!   become 0), a visible seam at the arena's centre lines; rounding
//!   keeps the lattice uniform.
//!
//! The wire value is also the change test of the kit's per-team ledger:
//! two positions within the same centimetre produce no new snapshot.

use bevy_ecs::prelude::Changed;
use bytes::BytesMut;
use prost::Message;

use crate::arena::UnitRecord;
use crate::components::Pos3;
use gsb_kit::codec::RecordCodec;

/// A position quantized to integer centimetres — the arena's wire value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Cm3 {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// Metres → centimetres, rounded to nearest (saturating outside `i32`).
#[inline]
#[must_use]
pub fn to_cm(metres: f32) -> i32 {
    (metres * 100.0).round() as i32
}

/// Centimetres → metres.
#[inline]
#[must_use]
pub fn from_cm(cm: i32) -> f32 {
    cm as f32 / 100.0
}

impl From<Pos3> for Cm3 {
    #[inline]
    fn from(p: Pos3) -> Self {
        Self {
            x: to_cm(p.x),
            y: to_cm(p.y),
            z: to_cm(p.z),
        }
    }
}

/// The arena's record codec — zero-sized (the quantization is fixed).
#[derive(Debug, Clone, Copy, Default)]
pub struct ArenaCodec;

impl RecordCodec for ArenaCodec {
    type Marker = Pos3;
    type Query = &'static Pos3;
    type Dirty = Changed<Pos3>;
    type Wire = Cm3;

    #[inline]
    fn wire(&self, pos: &Pos3) -> Cm3 {
        Cm3::from(*pos)
    }

    #[inline]
    fn encode(&self, id: u64, &Cm3 { x, y, z }: &Cm3, out: &mut BytesMut) {
        UnitRecord {
            entity: id,
            x,
            y,
            z,
        }
        .encode(out)
        .expect("protobuf encode into an in-memory buffer failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rounded to the nearest centimetre, symmetric around zero, and
    /// saturating far outside the arena.
    #[test]
    fn quantization_rounds_to_the_nearest_centimetre() {
        let q = |x, y, z| Cm3::from(Pos3::new(x, y, z));
        let cm = |x, y, z| Cm3 { x, y, z };
        assert_eq!(q(1.234, 5.678, -9.996), cm(123, 568, -1000));
        assert_eq!(q(0.004, -0.004, 0.006), cm(0, 0, 1));
        assert_eq!(q(-0.006, 30.0, -50.0), cm(-1, 3000, -5000));
        assert_eq!(to_cm(1.0e12), i32::MAX);
        assert_eq!(from_cm(-1250), -12.5);
    }

    /// The codec's record body is exactly the typed `UnitRecord`
    /// encoding (the client decodes it with the typed mirror).
    #[test]
    fn record_body_is_the_unit_record_encoding() {
        const SAMPLES: [i32; 7] = [0, 1, -1, 63, -64, 5_000, i32::MIN];
        for id in [1u64, 127, 128, u64::MAX] {
            for (i, x) in SAMPLES.into_iter().enumerate() {
                let (y, z) = (SAMPLES[(i + 2) % 7], SAMPLES[(i + 5) % 7]);
                let mut out = BytesMut::new();
                ArenaCodec.encode(id, &Cm3 { x, y, z }, &mut out);
                let typed = UnitRecord {
                    entity: id,
                    x,
                    y,
                    z,
                };
                assert_eq!(
                    &out[..],
                    &typed.encode_to_vec()[..],
                    "({id}, {x}, {y}, {z})"
                );
            }
        }
    }
}
