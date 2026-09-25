//! The record framing of a snapshot (KIT-ARCHITECTURE §4.1/§5, "A31"):
//! how a frame's records land in the envelope, in the game's framing
//! ([`RecordCodec::RUN`]).
//!
//! - **`entities`** (the default): each record is one length-delimited
//!   `entities` entry (field 2, `0x12` + length + body) — byte for byte
//!   what every kit room wrote before the record run existed.
//! - **the record run**: the frame's records back to back in ONE
//!   `records` field (6): `0x32` + the run's length, then per record its
//!   wire id (varint) and the game's self-delimiting body.
//!
//! A frame's records are written between [`Records::open`] and
//! [`Records::close`] — the region is empty in the `entities` framing
//! and the `records` field in the run (its length slot is fixed up at
//! the close, the way [`put_delimited`] does it, so the records are
//! written straight into the frame without a first sizing pass). The
//! shared cell pieces of the spatial rooms are records WITHOUT the
//! region ([`encode_entity_records`]); the group frame assembling them
//! opens one region around all of them.

use bytes::{BufMut, Bytes, BytesMut};
use prost::encoding::varint::{encode_varint, encoded_len_varint};

use super::put_delimited;
use crate::codec::RecordCodec;

/// `entities` (field 2), length-delimited.
const TAG_ENTITIES: u8 = 0x12;
/// `records` (field 6), length-delimited: the record run.
const TAG_RECORDS: u8 = 0x32;

/// A frame's record region (module docs). Close it after the records.
#[must_use = "a record run must be closed"]
pub(crate) struct Records {
    /// The run's length slot (one byte, after the tag); `None` in the
    /// `entities` framing (nothing to open or close).
    slot: Option<usize>,
}

impl Records {
    /// Open the region: nothing in the `entities` framing, the `records`
    /// tag and a one-byte length slot in the run (`run`).
    #[inline]
    pub(crate) fn open(run: bool, out: &mut BytesMut) -> Self {
        if !run {
            return Self { slot: None };
        }
        out.put_u8(TAG_RECORDS);
        out.put_u8(0);
        Self {
            slot: Some(out.len() - 1),
        }
    }

    /// Close the region: in the run, write the run's length into its
    /// slot — or remove the field when no record was written (an empty
    /// run is never written: a frame without records is the same bytes
    /// in both framings). A run of 128 bytes or more moves right in
    /// place to make room for its longer length.
    #[inline]
    pub(crate) fn close(self, out: &mut BytesMut) {
        let Some(slot) = self.slot else {
            return;
        };
        let len = out.len() - slot - 1;
        if len == 0 {
            out.truncate(slot - 1);
        } else if len < 0x80 {
            out[slot] = len as u8;
        } else {
            let n = encoded_len_varint(len as u64);
            out.resize(out.len() + n - 1, 0);
            out.copy_within(slot + 1..slot + 1 + len, slot + n);
            let mut prefix = &mut out[slot..slot + n];
            encode_varint(len as u64, &mut prefix);
        }
    }
}

/// Append `records` as one frame's records (the whole region, opened
/// and closed): one record each, in iteration order.
pub(crate) fn put_entity_records<'a, R: RecordCodec>(
    codec: &R,
    records: impl Iterator<Item = (u64, &'a R::Wire)>,
    out: &mut BytesMut,
) {
    let region = Records::open(R::RUN, out);
    for (id, wire) in records {
        put_entity_record(codec, id, wire, out);
    }
    region.close(out);
}

/// Append record `id` with value `wire`, framed as the codec frames:
/// an `entities` entry around [`RecordCodec::encode`]'s body, or — in
/// the run — the id followed by the body (inside an open region).
#[inline]
pub(crate) fn put_entity_record<R: RecordCodec>(
    codec: &R,
    id: u64,
    wire: &R::Wire,
    out: &mut BytesMut,
) {
    if R::RUN {
        encode_varint(id, out);
        codec.encode(id, wire, out);
    } else {
        put_delimited(out, TAG_ENTITIES, |o| codec.encode(id, wire, o));
    }
}

/// Append record `id` whose body is already encoded — a record another
/// shard encoded with the game's codec (the team exchange's imported
/// records, `docs/CROSS-SHARD.md` §8b): the same bytes
/// [`put_entity_record`] writes for the same record, in `R`'s framing.
#[inline]
pub(crate) fn put_entity_body<R: RecordCodec>(id: u64, body: &[u8], out: &mut BytesMut) {
    if R::RUN {
        encode_varint(id, out);
        out.extend_from_slice(body);
    } else {
        put_delimited(out, TAG_ENTITIES, |o| o.extend_from_slice(body));
    }
}

/// How a delta engine writes one record of value `W`: through the
/// game's codec for its own wire values (the blanket impl below — every
/// room but one), or through a writer that also knows pre-encoded
/// records (the sharded team composite's imports). `RUN` is the
/// framing ([`RecordCodec::RUN`] of the game's codec); [`Self::due`]
/// the record's send-rate schedule (A10).
pub(crate) trait WriteRecord<W> {
    /// Whether the records ride the record run.
    const RUN: bool;

    /// Append record `id` with value `wire` (inside an open region).
    fn put(&self, id: u64, wire: &W, out: &mut BytesMut);

    /// Whether record `id`, CHANGED from `held` (what its clients hold)
    /// to `now`, is due on `step` — its class's schedule
    /// ([`RecordCodec::send_every`],
    /// [`SendEvery::due`](crate::codec::SendEvery::due)).
    fn due(&self, step: u64, id: u64, held: &W, now: &W) -> bool;
}

impl<R: RecordCodec> WriteRecord<R::Wire> for R {
    const RUN: bool = R::RUN;

    #[inline]
    fn put(&self, id: u64, wire: &R::Wire, out: &mut BytesMut) {
        put_entity_record(self, id, wire, out);
    }

    #[inline]
    fn due(&self, step: u64, id: u64, _held: &R::Wire, now: &R::Wire) -> bool {
        self.send_every(now).due(step, id)
    }
}

/// A piece of records WITHOUT the region (a spatial room's per-cell
/// piece, shared by reference by every group frame that shows the cell
/// — the group frame opens the one region around its pieces).
pub(crate) fn encode_entity_records<'a, R: RecordCodec>(
    codec: &R,
    records: impl Iterator<Item = (u64, &'a R::Wire)>,
) -> Bytes {
    let mut out = BytesMut::new();
    for (id, wire) in records {
        put_entity_record(codec, id, wire, &mut out);
    }
    out.freeze()
}

#[cfg(test)]
mod tests;
