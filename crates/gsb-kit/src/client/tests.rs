//! The reference client on hand-built frames: the fixture game's record
//! and `Grid2`'s cell exit (the fixture's typed mirror decodes them),
//! frames written both by the KIT's own server-side writers (unpacked
//! `removed`, the bytes a room ships) and by a generated encoder (packed
//! `removed`, fields in any order).

use bytes::BytesMut;
use prost::Message;

use super::*;
use crate::common::{
    encode_cell_exit, encode_entity_exits, put_entity_records, write_snapshot_header,
};
use crate::proto;
use crate::space::{Cell, CellSpace, Grid2};
use crate::testing::{CellExit, FixCodec, Record, WirePos};

mod rules;
mod session;
mod wire;

/// The cell edge every test uses (wire units).
const CELL: f32 = 20.0;

/// The fixture game's decode seam: a record → its truncated position,
/// whose cell is `Grid2`'s; a cell exit → `Grid2`'s cell.
#[derive(Debug, Clone, Copy)]
struct FixDecoder(Grid2);

impl Default for FixDecoder {
    fn default() -> Self {
        Self(Grid2::new(CELL))
    }
}

impl ClientDecoder for FixDecoder {
    type Record = (i32, i32);
    type Cell = Cell;

    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32)), ClientError> {
        let r = Record::decode(body)?;
        Ok((r.entity, (r.x, r.y)))
    }

    fn cell_of(&self, &(x, y): &(i32, i32)) -> Cell {
        CellSpace::<WirePos>::cell_of(&self.0, &WirePos { x, y })
    }

    fn cell_exit(&self, body: &[u8]) -> Result<Cell, ClientError> {
        let c = CellExit::decode(body)?;
        Ok(Cell(c.x, c.y))
    }
}

type View = ClientView<FixDecoder>;

/// One snapshot, before encoding.
#[derive(Debug, Clone, Default)]
struct Frame {
    sequence: u64,
    delta: bool,
    /// `(wire id, x, y)` upserts (a full: the whole view).
    records: Vec<(u64, i32, i32)>,
    removed: Vec<u64>,
    exits: Vec<Cell>,
}

impl Frame {
    fn full(sequence: u64, records: &[(u64, i32, i32)]) -> Self {
        Self {
            sequence,
            records: records.to_vec(),
            ..Self::default()
        }
    }

    fn delta(sequence: u64, records: &[(u64, i32, i32)]) -> Self {
        Self {
            delta: true,
            ..Self::full(sequence, records)
        }
    }

    fn removing(mut self, ids: &[u64]) -> Self {
        self.removed = ids.to_vec();
        self
    }

    fn exiting(mut self, cells: &[Cell]) -> Self {
        self.exits = cells.to_vec();
        self
    }

    /// The bytes a kit room writes: header, `removed`, `cell_exits`,
    /// then `entities` (the kit's own writers, unpacked `removed`).
    fn kit(&self) -> Vec<u8> {
        let mut out = BytesMut::new();
        write_snapshot_header(&mut out, self.sequence, self.delta);
        out.extend_from_slice(&encode_entity_exits(&self.removed));
        for &cell in &self.exits {
            let grid = Grid2::new(CELL);
            out.extend_from_slice(&encode_cell_exit::<WirePos, _>(&grid, cell));
        }
        let wires: Vec<(u64, WirePos)> = self
            .records
            .iter()
            .map(|&(id, x, y)| (id, WirePos { x, y }))
            .collect();
        put_entity_records(&FixCodec, wires.iter().map(|(id, w)| (*id, w)), &mut out);
        out.to_vec()
    }

    /// The same content through a generated encoder: packed `removed`,
    /// and the proto field order (`entities` BEFORE `removed` and
    /// `cell_exits` — the application order is the rules', not the
    /// wire's).
    fn generated(&self) -> Vec<u8> {
        self.message().encode_to_vec()
    }

    fn message(&self) -> proto::WorldSnapshot {
        proto::WorldSnapshot {
            sequence: self.sequence,
            entities: self
                .records
                .iter()
                .map(|&(entity, x, y)| Record { entity, x, y }.encode_to_vec())
                .collect(),
            removed: self.removed.clone(),
            cell_exits: self
                .exits
                .iter()
                .map(|c| CellExit { x: c.0, y: c.1 }.encode_to_vec())
                .collect(),
            delta: self.delta,
        }
    }

    /// This frame as the one-shot `Private` snapshot.
    fn private(&self) -> Vec<u8> {
        proto::Private {
            payload: Some(proto::private::Payload::Snapshot(self.message())),
            ..Default::default()
        }
        .encode_to_vec()
    }
}

/// Both encodings of every frame give the same outcome and the same view.
fn feed(frames: &[Frame]) -> (View, Vec<Apply>) {
    let (mut kit, mut generated) = (View::default(), View::default());
    let mut outcomes = Vec::new();
    for f in frames {
        let a = kit.apply_snapshot(&f.kit()).expect("kit frame decodes");
        let b = generated
            .apply_snapshot(&f.generated())
            .expect("generated frame decodes");
        assert_eq!(a, b, "both encodings, one outcome: {f:?}");
        assert_eq!(sorted(&kit), sorted(&generated), "both encodings: {f:?}");
        assert_eq!(a.sequence, f.sequence);
        outcomes.push(a.apply);
    }
    assert_eq!(kit.counters(), generated.counters());
    (kit, outcomes)
}

/// The view as sorted `(wire id, x, y)`.
fn sorted(view: &View) -> Vec<(u64, i32, i32)> {
    let mut v: Vec<_> = view.iter().map(|(id, &(x, y))| (id, x, y)).collect();
    v.sort_unstable();
    v
}
