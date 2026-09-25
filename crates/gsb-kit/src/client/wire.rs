//! A zero-copy walk over one protobuf message: its fields as `(number,
//! value)` pairs borrowed from the buffer, with no allocation. The view
//! reads `WorldSnapshot`, `Private` and `InputAck` with it and hands each
//! record / cell-exit BODY to the game's decoder as a sub-slice of the
//! frame; a game's decoder may walk that body with it too (a hand
//! decoder of a small record skips a generated type's per-message setup
//! — what a load generator's receive loop, run for thousands of clients,
//! pays per record).
//!
//! It accepts everything a protobuf parser accepts (fields in any order,
//! repeated scalars packed or unpacked, unknown fields of every wire type
//! skipped by the caller) and rejects what a parser rejects (a truncated
//! field, a varint longer than ten bytes, field number 0). Groups (wire
//! types 3/4) are rejected: proto3 cannot declare one.

use super::ClientError;

/// Malformed protobuf: why the walk stopped (a plain value — cheap to
/// carry through a hot loop; `?` turns it into
/// [`ClientError::Malformed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl From<Malformed> for ClientError {
    #[inline(always)]
    fn from(m: Malformed) -> Self {
        Self::Malformed(m.0)
    }
}

/// One field's value, borrowed from the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    /// Wire type 0.
    Varint(u64),
    /// Wire type 1.
    Fixed64(u64),
    /// Wire type 2: the body (a sub-message, bytes, or a packed run).
    Len(&'a [u8]),
    /// Wire type 5.
    Fixed32(u32),
}

/// The fields of one message, in wire order. Yields at most one error,
/// then ends.
pub struct Fields<'a> {
    buf: &'a [u8],
}

impl<'a> Fields<'a> {
    /// Walk `buf` (one whole message).
    #[inline(always)]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// How many bytes are left to walk.
    #[inline(always)]
    pub(super) fn remaining(&self) -> usize {
        self.buf.len()
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let (head, rest) = self
            .buf
            .split_first_chunk::<N>()
            .ok_or(Malformed("truncated field"))?;
        self.buf = rest;
        Ok(*head)
    }

    #[inline(always)]
    fn field(&mut self) -> Result<(u32, Value<'a>), Malformed> {
        let key = varint(&mut self.buf)?;
        let number = u32::try_from(key >> 3)
            .ok()
            .filter(|&n| n != 0)
            .ok_or(Malformed("invalid field number"))?;
        let value = match key & 7 {
            0 => Value::Varint(varint(&mut self.buf)?),
            1 => Value::Fixed64(u64::from_le_bytes(self.take()?)),
            2 => {
                let len = usize::try_from(varint(&mut self.buf)?)
                    .ok()
                    .filter(|&len| len <= self.buf.len())
                    .ok_or(Malformed("truncated field"))?;
                let (body, rest) = self.buf.split_at(len);
                self.buf = rest;
                Value::Len(body)
            }
            5 => Value::Fixed32(u32::from_le_bytes(self.take()?)),
            _ => return Err(Malformed("unsupported wire type")),
        };
        Ok((number, value))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Value<'a>), Malformed>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        let field = self.field();
        if field.is_err() {
            self.buf = &[];
        }
        Some(field)
    }
}

/// A `sint32` field's value (zigzag) from its varint, as protobuf
/// decodes it (the varint truncated to 32 bits first).
#[inline(always)]
#[must_use]
pub fn sint32(varint: u64) -> i32 {
    let v = varint as u32;
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

/// Read one base-128 varint off the front of `buf` (at most ten bytes;
/// the tenth may only carry the top bit of a `u64`) and advance `buf`
/// past it — the building block a game's record-run decoder
/// ([`ClientDecoder::run_record`](super::ClientDecoder::run_record))
/// reads its own varints with.
///
/// # Errors
///
/// [`Malformed`] when `buf` ends inside the varint or it is longer than
/// a `u64`.
#[inline(always)]
pub fn varint(buf: &mut &[u8]) -> Result<u64, Malformed> {
    // Fast path: keys, lengths and small values are one byte.
    if let Some((&byte, rest)) = buf.split_first()
        && byte < 0x80
    {
        *buf = rest;
        return Ok(u64::from(byte));
    }
    let mut value = 0u64;
    for (i, &byte) in buf.iter().enumerate().take(10) {
        if i == 9 && byte > 1 {
            break;
        }
        value |= u64::from(byte & 0x7F) << (7 * i);
        if byte < 0x80 {
            *buf = &buf[i + 1..];
            return Ok(value);
        }
    }
    Err(Malformed("invalid varint"))
}

/// A varint-typed field's values: one for the unpacked form (`Varint`),
/// a whole run for the packed form (`Len`) — a repeated scalar may use
/// either, and a parser accepts both.
pub(super) fn each_varint(value: Value<'_>, mut push: impl FnMut(u64)) -> Result<(), Malformed> {
    match value {
        Value::Varint(v) => push(v),
        Value::Len(mut run) => {
            while !run.is_empty() {
                push(varint(&mut run)?);
            }
        }
        _ => return Err(Malformed("wrong wire type")),
    }
    Ok(())
}

/// The last `processed_up_to` (field 1) of one `InputAck` body (0 when
/// absent, as proto3 omits it).
pub(super) fn input_ack(body: &[u8]) -> Result<u64, Malformed> {
    let mut up_to = 0;
    for field in Fields::new(body) {
        match field? {
            (1, Value::Varint(v)) => up_to = v,
            (1, _) => return Err(Malformed("wrong wire type")),
            _ => {}
        }
    }
    Ok(up_to)
}
