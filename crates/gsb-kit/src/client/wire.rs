//! A zero-copy walk over one protobuf message: the kit envelope's fields
//! as `(number, value)` pairs borrowed from the frame, with no
//! allocation. The view reads `WorldSnapshot`, `Private` and `InputAck`
//! with it and hands each record / cell-exit BODY to the game's decoder
//! as a sub-slice of the frame.
//!
//! It accepts everything a protobuf parser accepts for these messages
//! (fields in any order, repeated scalars packed or unpacked, unknown
//! fields of every wire type skipped) and rejects what a parser rejects
//! (a truncated field, a varint longer than ten bytes, field number 0).
//! Groups (wire types 3/4) are rejected: proto3 cannot declare one, so
//! no kit envelope or typed mirror can carry one.

use super::ClientError;

/// One field's value, borrowed from the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Value<'a> {
    /// Wire type 0.
    Varint(u64),
    /// Wire type 2: the body (a sub-message, bytes, or a packed run).
    Len(&'a [u8]),
    /// Wire types 1 and 5 (no kit field uses them; skipped by callers).
    Fixed,
}

/// The fields of one message, in wire order. Yields at most one error,
/// then ends.
pub(super) struct Fields<'a> {
    buf: &'a [u8],
}

impl<'a> Fields<'a> {
    pub(super) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ClientError> {
        if self.buf.len() < n {
            return Err(ClientError::Envelope("truncated field"));
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn field(&mut self) -> Result<(u32, Value<'a>), ClientError> {
        let key = varint(&mut self.buf)?;
        let number = u32::try_from(key >> 3)
            .ok()
            .filter(|&n| n != 0)
            .ok_or(ClientError::Envelope("invalid field number"))?;
        let value = match key & 7 {
            0 => Value::Varint(varint(&mut self.buf)?),
            1 => {
                self.take(8)?;
                Value::Fixed
            }
            2 => {
                let len = usize::try_from(varint(&mut self.buf)?)
                    .map_err(|_| ClientError::Envelope("truncated field"))?;
                Value::Len(self.take(len)?)
            }
            5 => {
                self.take(4)?;
                Value::Fixed
            }
            _ => return Err(ClientError::Envelope("unsupported wire type")),
        };
        Ok((number, value))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Value<'a>), ClientError>;

    #[inline]
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

/// Read one base-128 varint off the front of `buf` (at most ten bytes;
/// the tenth may only carry the top bit of a `u64`).
#[inline]
pub(super) fn varint(buf: &mut &[u8]) -> Result<u64, ClientError> {
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
    Err(ClientError::Envelope("invalid varint"))
}

/// A varint-typed field's values: one for the unpacked form (`Varint`),
/// a whole run for the packed form (`Len`) — a repeated scalar may use
/// either, and a parser accepts both.
pub(super) fn each_varint(value: Value<'_>, mut push: impl FnMut(u64)) -> Result<(), ClientError> {
    match value {
        Value::Varint(v) => push(v),
        Value::Len(mut run) => {
            while !run.is_empty() {
                push(varint(&mut run)?);
            }
        }
        Value::Fixed => return Err(ClientError::Envelope("wrong wire type")),
    }
    Ok(())
}

/// The last `processed_up_to` (field 1) of one `InputAck` body (0 when
/// absent, as proto3 omits it).
pub(super) fn input_ack(body: &[u8]) -> Result<u64, ClientError> {
    let mut up_to = 0;
    for field in Fields::new(body) {
        match field? {
            (1, Value::Varint(v)) => up_to = v,
            (1, _) => return Err(ClientError::Envelope("wrong wire type")),
            _ => {}
        }
    }
    Ok(up_to)
}
