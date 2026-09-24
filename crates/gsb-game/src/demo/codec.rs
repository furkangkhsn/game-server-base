//! The demo's [`RecordCodec`] (KIT-ARCHITECTURE §4.1): an entity is
//! broadcast iff it has a [`Position`]; its record is the TRUNCATED
//! position, `(x as i32, y as i32)`, written as `game.proto`'s
//! `EntityRecord { uint64 entity = 1; sint32 x = 2; sint32 y = 3; }`.

use bevy_ecs::prelude::Changed;
use bytes::BytesMut;
use prost::Message;

use crate::demo::components::Position;
use crate::demo::game::EntityRecord;
use crate::kit::codec::RecordCodec;

/// The demo's record codec — a zero-sized value: the quantization is
/// fixed (truncation to the integer wire lattice), so there is no
/// instance data.
#[derive(Debug, Clone, Copy, Default)]
pub struct DemoCodec;

impl RecordCodec for DemoCodec {
    type Marker = Position;
    type Query = &'static Position;
    type Dirty = Changed<Position>;
    /// The truncated wire position (the same value the sharded rooms'
    /// border strip carries as `StripPos`).
    type Wire = (i32, i32);

    #[inline]
    fn wire(&self, pos: &Position) -> (i32, i32) {
        (pos.x as i32, pos.y as i32)
    }

    #[inline]
    fn encode(&self, id: u64, &(x, y): &(i32, i32), out: &mut BytesMut) {
        EntityRecord { entity: id, x, y }
            .encode(out)
            .expect("protobuf encode into an in-memory buffer failed");
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use prost::Message;

    use super::*;
    use crate::demo::game::CellExit;
    use crate::kit::space::{Cell, CellSpace, Grid2};

    const SAMPLES: [i32; 9] = [0, 1, -1, 63, -64, 64, 1_000, -70_000, i32::MIN];

    /// The codec's record body is exactly the typed `EntityRecord`
    /// encoding (the client decodes it with the typed mirror).
    #[test]
    fn record_body_is_the_entity_record_encoding() {
        for id in [1u64, 127, 128, u64::MAX] {
            for x in SAMPLES {
                for y in SAMPLES {
                    let mut out = BytesMut::new();
                    DemoCodec.encode(id, &(x, y), &mut out);
                    let typed = EntityRecord { entity: id, x, y }.encode_to_vec();
                    assert_eq!(&out[..], &typed[..], "({id}, {x}, {y})");
                }
            }
        }
    }

    /// The kit's `Grid2` preset writes the `CellExit` body byte-for-byte
    /// as `game.proto`'s typed `CellExit` (proto3 zero omission
    /// included) — the demo's choice of the preset keeps the wire.
    #[test]
    fn grid2_cell_exit_body_is_the_typed_cell_exit() {
        let grid = Grid2::new(20.0);
        for x in SAMPLES {
            for y in SAMPLES {
                let mut out = BytesMut::new();
                grid.encode_cell(Cell(x, y), &mut out);
                let typed = CellExit { x, y }.encode_to_vec();
                assert_eq!(&out[..], &typed[..], "Cell({x}, {y})");
            }
        }
    }
}
