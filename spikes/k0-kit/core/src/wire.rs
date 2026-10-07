//! The record seam, ECS-free: a component (or any plain value) that knows
//! its wire value, and a wire value that knows its bytes.
//!
//! In `gsb-kit` today `RecordCodec` carries three bevy types (`Marker:
//! Component`, `Query: ReadOnlyQueryData`, `Dirty: QueryFilter`). Here
//! the core keeps only what is not an ECS question — the wire value and
//! its encoding; WHICH components feed it and WHEN they changed is the
//! binding's (`docs/KIT-ECS.md` §5.2).

/// A value the record is built from (the demo: `Position`).
pub trait ToWire {
    /// The quantized wire value.
    type Wire: WireValue;
    /// The value's wire form.
    fn wire(&self) -> Self::Wire;
}

/// A record's typed wire value: equality is the change test.
pub trait WireValue: Clone + Eq + std::fmt::Debug + Send + 'static {
    /// Append the record body (self-delimiting — the record run).
    fn encode(&self, out: &mut Vec<u8>);
}

/// Append `v` as a protobuf varint.
pub(crate) fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Append `v` zig-zag encoded (sint32).
pub fn zigzag(v: i32, out: &mut Vec<u8>) {
    varint(((v << 1) ^ (v >> 31)) as u32 as u64, out);
}
