//! The kit's snapshot envelope, written by hand (KIT-ARCHITECTURE §5):
//! `WorldSnapshot { uint64 sequence = 1; repeated bytes entities = 2;
//! repeated uint64 removed = 3; repeated bytes cell_exits = 4; bool
//! delta = 5; }`. The record and cell-exit BODIES come from the game
//! ([`RecordCodec::encode`], [`CellSpace::encode_cell`]); the tags, the
//! lengths, the header and the field ordering are the kit's.
//!
//! A `repeated bytes` field and a `repeated <Message>` field with the
//! same number produce identical bytes (both length-delimited), so the
//! demo's typed `game.proto` mirror (`EntityRecord`, `CellExit`) decodes
//! these frames unchanged.

use bytes::{BufMut, Bytes, BytesMut};
use prost::encoding::varint::encode_varint;

use crate::codec::RecordCodec;
use crate::space::CellSpace;

/// `entities` (field 2), length-delimited.
const TAG_ENTITIES: u8 = 0x12;
/// `removed` (field 3), varint (one entry per id).
const TAG_REMOVED: u8 = 0x18;
/// `cell_exits` (field 4), length-delimited.
const TAG_CELL_EXITS: u8 = 0x22;

/// Append one length-delimited field: `tag`, the body's length as a
/// varint, then the body `write` appends. The body is written straight
/// into `out` behind a one-byte length slot (the common case: every
/// record under 128 bytes costs no copy); a longer body is moved right
/// to make room for the longer varint.
#[inline]
pub(crate) fn put_delimited(out: &mut BytesMut, tag: u8, write: impl FnOnce(&mut BytesMut)) {
    out.put_u8(tag);
    let at = out.len();
    out.put_u8(0);
    write(out);
    let len = out.len() - at - 1;
    if len < 0x80 {
        out[at] = len as u8;
    } else {
        let body = out[at + 1..].to_vec();
        out.truncate(at);
        encode_varint(len as u64, out);
        out.extend_from_slice(&body);
    }
}

/// The cell-delta snapshot header: `sequence` (field 1, varint — always
/// written) + the `delta` flag (field 5; written only when true — a
/// `false`/absent flag means FULL, per `game.proto`).
pub(crate) fn write_snapshot_header(buf: &mut BytesMut, tick: u64, delta: bool) {
    buf.put_u8(0x08); // field 1 (sequence), varint
    encode_varint(tick, buf);
    if delta {
        buf.put_u8(0x28); // field 5 (delta), varint
        buf.put_u8(1);
    }
}

/// The header of a whole-world FULL snapshot, byte-identical to the
/// proto3 encoding of a `WorldSnapshot` with only `sequence` and
/// `entities` set: `sequence` is omitted when 0 (the terminal
/// `match_result` snapshot), and `delta = false` is never written.
pub(crate) fn write_full_header(buf: &mut BytesMut, sequence: u64) {
    if sequence != 0 {
        buf.put_u8(0x08); // field 1 (sequence), varint
        encode_varint(sequence, buf);
    }
}

/// Append `records` as `entities` entries (field 2): one
/// length-delimited [`RecordCodec::encode`] body each, in iteration
/// order.
pub(crate) fn put_entity_records<'a, R: RecordCodec>(
    codec: &R,
    records: impl Iterator<Item = (u64, &'a R::Wire)>,
    out: &mut BytesMut,
) {
    for (id, wire) in records {
        put_delimited(out, TAG_ENTITIES, |o| codec.encode(id, wire, o));
    }
}

/// [`put_entity_records`] as one frozen piece, shareable by reference.
pub(crate) fn encode_entity_records<'a, R: RecordCodec>(
    codec: &R,
    records: impl Iterator<Item = (u64, &'a R::Wire)>,
) -> Bytes {
    let mut out = BytesMut::new();
    put_entity_records(codec, records, &mut out);
    out.freeze()
}

/// Encode `exits` as `removed` entries (field 3, varint) — the
/// entity-exit piece of a delta.
pub(crate) fn encode_entity_exits(exits: &[u64]) -> Bytes {
    let mut out = BytesMut::new();
    for &wire in exits {
        out.put_u8(TAG_REMOVED);
        encode_varint(wire, &mut out);
    }
    out.freeze()
}

/// Encode one `cell_exits` entry (field 4, length-delimited) for `cell`
/// — the single record that makes the client forget a whole cell.
pub(crate) fn encode_cell_exit<W, S: CellSpace<W>>(space: &S, cell: S::Cell) -> Bytes {
    let mut out = BytesMut::new();
    put_delimited(&mut out, TAG_CELL_EXITS, |o| space.encode_cell(cell, o));
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The length slot matches a minimal protobuf varint on both sides
    /// of the one-byte boundary.
    #[test]
    fn put_delimited_writes_minimal_lengths() {
        for len in [0usize, 1, 127, 128, 300, 20_000] {
            let mut out = BytesMut::new();
            put_delimited(&mut out, 0x12, |o| o.extend_from_slice(&vec![0xAB; len]));
            let mut expected = vec![0x12];
            let mut l = BytesMut::new();
            encode_varint(len as u64, &mut l);
            expected.extend_from_slice(&l);
            expected.extend_from_slice(&vec![0xAB; len]);
            assert_eq!(&out[..], &expected[..], "body length {len}");
        }
    }
}
