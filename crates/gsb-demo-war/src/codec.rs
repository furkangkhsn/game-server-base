//! The war game's [`RecordCodec`] (KIT-ARCHITECTURE §4.1): every unit
//! with a [`Pos3`] is broadcast; its record is the position QUANTIZED to
//! integer decimetres plus its [`Unit`] — [`WarWire`], written as
//! `war.proto`'s `UnitRecord { entity = 1; x, y, z = 2..4; kind = 5;
//! faction = 6; hp = 7 }`. `Dirty` is position OR unit changed (a
//! player losing hit points, a point changing hands, is news).
//!
//! **Decimetres, rounded to nearest (`i32`)** — the MMO's choice for the
//! same reasons: on the 1 600 m map every ground coordinate is at most
//! ±8 000 dm, a 2-byte zig-zag varint; a player running 7 m/s is a new
//! value every 30 Hz tick, a standing one costs nothing; rounding (not
//! truncation) keeps the cell at the seams x = 0 and z = 0 as wide as
//! any other. A unit on the ground (`y` = 0) and faction 0 are proto3
//! defaults: not written.
//!
//! **The faction on the wire is 1-based** (`0` = none): the team fog
//! frame does not say which records are allies — the record does.
//!
//! **The wire's `Planar` is in METRES** (`floor(dm / 10)`, the
//! position's unit — the kit's `Planar` unit contract, which
//! `GridPartition2::admits` relies on).

use bevy_ecs::prelude::{Changed, Or};
use bytes::BytesMut;
use gsb_kit::codec::RecordCodec;
use gsb_kit::space::Planar;
use gsb_kit::team::Team;
use prost::Message;

use crate::components::{Kind, Pos3, Unit};
use crate::war::UnitRecord;

/// One unit's quantized record — the game's wire value (and the border
/// strip's payload: `Strip = Wire`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WarWire {
    /// Position, decimetres.
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub kind: Kind,
    /// The faction, 1-based; 0 = none (the wire's numbering).
    pub faction: u8,
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

/// A faction as the wire numbers it: `team + 1`, 0 for none.
#[inline]
#[must_use]
pub fn wire_faction(faction: Option<Team>) -> u8 {
    faction.map_or(0, |t| t.0 + 1)
}

/// The wire's faction number back as a team (`None` for 0).
#[inline]
#[must_use]
pub fn team_of_wire(faction: u32) -> Option<Team> {
    u8::try_from(faction)
        .ok()
        .and_then(|f| f.checked_sub(1))
        .map(Team)
}

/// The ground-plane projection of the wire value, in whole METRES (the
/// position's unit, which `GridPartition2::admits` assumes).
impl Planar for WarWire {
    type Coord = i32;

    #[inline]
    fn planar(&self) -> [i32; 2] {
        [self.x.div_euclid(10), self.z.div_euclid(10)]
    }
}

impl WarWire {
    /// The quantized record of a unit at `pos`.
    #[must_use]
    pub fn of(pos: &Pos3, unit: &Unit) -> Self {
        Self {
            x: to_dm(pos.x),
            y: to_dm(pos.y),
            z: to_dm(pos.z),
            kind: unit.kind,
            faction: wire_faction(unit.faction),
            hp: unit.hp,
        }
    }

    /// The record's position, metres.
    #[must_use]
    pub fn pos(&self) -> Pos3 {
        Pos3::new(from_dm(self.x), from_dm(self.y), from_dm(self.z))
    }
}

/// The war game's record codec — zero-sized (the quantization is fixed).
#[derive(Debug, Clone, Copy, Default)]
pub struct WarCodec;

impl RecordCodec for WarCodec {
    type Marker = Pos3;
    type Query = (&'static Pos3, &'static Unit);
    type Dirty = Or<(Changed<Pos3>, Changed<Unit>)>;
    type Wire = WarWire;

    #[inline]
    fn wire(&self, (pos, unit): (&Pos3, &Unit)) -> WarWire {
        WarWire::of(pos, unit)
    }

    #[inline]
    fn encode(&self, id: u64, w: &WarWire, out: &mut BytesMut) {
        UnitRecord {
            entity: id,
            x: w.x,
            y: w.y,
            z: w.z,
            kind: w.kind as i32,
            faction: u32::from(w.faction),
            hp: u32::from(w.hp),
        }
        .encode(out)
        .expect("protobuf encode into an in-memory buffer failed");
    }
}

#[cfg(test)]
mod tests;
