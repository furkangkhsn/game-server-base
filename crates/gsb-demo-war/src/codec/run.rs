//! The war's record body in the kit's RECORD RUN (`RecordCodec::RUN`,
//! KIT-ARCHITECTURE §10 "A31"): tagless varints, self-delimiting — the
//! kit writes the unit's wire id in front of it, so the body does not
//! repeat it.
//!
//! ```text
//! head   varint   has_y | kind << 1 | faction << 3
//! x      varint   zigzag, decimetres
//! z      varint   zigzag, decimetres
//! y      varint   zigzag, decimetres — only when has_y (off the ground)
//! hp     varint
//! ```
//!
//! A walking player is 6 bytes (head 1, x 2, z 2, hp 1 — every ground
//! coordinate on the map is a 2-byte zigzag); a tower 8 (its platform
//! height). Every field is a varint, so any value round-trips (a kind or
//! faction out of the game's range only costs a longer head). The
//! game's own choice — the kit never looks inside a body.

use bytes::BytesMut;
use gsb_kit::client::ClientError;
use gsb_kit::client::wire::{Malformed, sint32, varint};
use prost::encoding::varint::encode_varint;

use super::WarWire;
use crate::components::Kind;
use crate::war::UnitRecord;

fn zigzag(v: i32) -> u64 {
    u64::from(((v << 1) ^ (v >> 31)) as u32)
}

/// Append the run body of `w` (module docs).
pub fn write_body(w: &WarWire, out: &mut BytesMut) {
    let has_y = u64::from(w.y != 0);
    encode_varint(
        has_y | (w.kind as u64) << 1 | u64::from(w.faction) << 3,
        out,
    );
    encode_varint(zigzag(w.x), out);
    encode_varint(zigzag(w.z), out);
    if w.y != 0 {
        encode_varint(zigzag(w.y), out);
    }
    encode_varint(u64::from(w.hp), out);
}

/// Read one run body off the front of `run` (advancing it past the
/// body).
///
/// # Errors
///
/// [`Malformed`] when the run ends inside the body, or the kind,
/// faction or hit points are out of range.
pub fn read_body(run: &mut &[u8]) -> Result<WarWire, Malformed> {
    let head = varint(run)?;
    let kind = match (head >> 1) & 3 {
        1 => Kind::Player,
        2 => Kind::Tower,
        3 => Kind::Point,
        _ => return Err(Malformed("a unit kind out of range")),
    };
    let faction = u8::try_from(head >> 3).map_err(|_| Malformed("a faction out of range"))?;
    let x = sint32(varint(run)?);
    let z = sint32(varint(run)?);
    let y = if head & 1 == 1 {
        sint32(varint(run)?)
    } else {
        0
    };
    let hp = u16::try_from(varint(run)?).map_err(|_| Malformed("hit points out of range"))?;
    Ok(WarWire {
        x,
        y,
        z,
        kind,
        faction,
        hp,
    })
}

/// One record of a war frame's run as a client keeps it: the unit
/// `entity` (the id the kit read off the run) and its body, as the
/// `UnitRecord` the war's clients store (the record's content — on the
/// wire it is the run body above, not a protobuf message). What a war
/// client's `ClientDecoder::run_record` returns.
///
/// # Errors
///
/// As [`read_body`].
pub fn read_record(entity: u64, run: &mut &[u8]) -> Result<UnitRecord, ClientError> {
    let w = read_body(run)?;
    Ok(UnitRecord {
        entity,
        x: w.x,
        y: w.y,
        z: w.z,
        kind: w.kind as i32,
        faction: u32::from(w.faction),
        hp: u32::from(w.hp),
    })
}
