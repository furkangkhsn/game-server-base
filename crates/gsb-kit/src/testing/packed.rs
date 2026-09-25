//! The fixture's record in the RECORD RUN (`RecordCodec::RUN`, A31):
//! the same wire value ([`WirePos`]) as [`super::FixCodec`], written as
//! a compact self-delimiting body — `x` then `y`, each a zigzag varint,
//! no tags and no id (the kit writes the id in front of it) — and the
//! client decoders of both framings the kit's tests apply frames with.

use bevy_ecs::prelude::Changed;
use bytes::BytesMut;
use prost::Message;
use prost::encoding::varint::encode_varint;

use crate::client::wire::{Malformed, sint32, varint};
use crate::client::{ClientDecoder, ClientError};
use crate::codec::RecordCodec;
use crate::space::{Cell, CellSpace, Grid2};

use super::{CellExit, FixCodec, Position, Record, WirePos};

/// The fixture's codec in the record run.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PackedCodec;

impl RecordCodec for PackedCodec {
    type Marker = Position;
    type Query = &'static Position;
    type Dirty = Changed<Position>;
    type Wire = WirePos;

    const RUN: bool = true;

    fn wire(&self, pos: &Position) -> WirePos {
        FixCodec.wire(pos)
    }

    fn encode(&self, _id: u64, wire: &WirePos, out: &mut BytesMut) {
        packed_body(wire, out);
    }
}

/// The packed body of `wire`: zigzag `x`, zigzag `y` (varints).
pub(crate) fn packed_body(wire: &WirePos, out: &mut BytesMut) {
    let zigzag = |v: i32| u64::from(((v << 1) ^ (v >> 31)) as u32);
    encode_varint(zigzag(wire.x), out);
    encode_varint(zigzag(wire.y), out);
}

/// Read one packed body off the front of `run`.
pub(crate) fn read_packed(run: &mut &[u8]) -> Result<WirePos, Malformed> {
    let x = sint32(varint(run)?);
    let y = sint32(varint(run)?);
    Ok(WirePos { x, y })
}

/// The fixture's decode seam in either framing (`RUN`): a record → its
/// wire position, whose cell is `Grid2`'s; a cell exit → `Grid2`'s cell.
/// `Dec<false>` is a decoder that did not opt into the run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Dec<const RUN: bool>(pub Grid2);

impl<const RUN: bool> Dec<RUN> {
    /// Cells of `edge` wire units (the rooms' AOI cell size).
    pub(crate) fn new(edge: f32) -> Self {
        Self(Grid2::new(edge))
    }
}

impl<const RUN: bool> ClientDecoder for Dec<RUN> {
    type Record = (i32, i32);
    type Cell = Cell;

    const RUN: bool = RUN;

    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32)), ClientError> {
        let r = Record::decode(body)?;
        Ok((r.entity, (r.x, r.y)))
    }

    fn run_record(&self, _id: u64, run: &mut &[u8]) -> Result<(i32, i32), ClientError> {
        let w = read_packed(run)?;
        Ok((w.x, w.y))
    }

    fn cell_of(&self, &(x, y): &(i32, i32)) -> Cell {
        CellSpace::<WirePos>::cell_of(&self.0, &WirePos { x, y })
    }

    fn cell_exit(&self, body: &[u8]) -> Result<Cell, ClientError> {
        let c = CellExit::decode(body)?;
        Ok(Cell(c.x, c.y))
    }
}
