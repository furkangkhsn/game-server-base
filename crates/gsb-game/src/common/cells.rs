//! The spatial cell grid the AOI rooms group by: the cell key, the
//! 3x3 block, and the per-cell ledgers that make one encoding serve
//! every group that can see it.

mod book;
mod pieces;

pub(crate) use book::*;
pub(crate) use pieces::*;

use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use prost::encoding::varint::encode_varint;

/// A spatial cell of the world grid — the AOI group key. Cell indices
/// are the floor of (wire position / `cell_size`) — see the module docs
/// of either room ("Cells are computed from the WIRE position").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell(pub i32, pub i32);

/// How many cells the visibility block extends in each direction from
/// the player's cell. `1` ⇒ a 3×3 block (the cell + its 8 ring-1
/// neighbors).
pub(crate) const RADIUS: i32 = 1;

/// The (dx, dy) offsets of the visibility block centered on a cell
/// (deterministic order: the assembly order of the group's packet).
pub(crate) const BLOCK_OFFSETS: [(i32, i32); 9] = [
    (-RADIUS, -RADIUS),
    (0, -RADIUS),
    (RADIUS, -RADIUS),
    (-RADIUS, 0),
    (0, 0),
    (RADIUS, 0),
    (-RADIUS, RADIUS),
    (0, RADIUS),
    (RADIUS, RADIUS),
];

/// The cell containing the WIRE (integer) position: `floor(x / cell_size)`
/// on the integer coordinates, so the client — which holds only wire
/// coordinates — computes the same cell.
#[inline]
pub(crate) fn cell_of(x: i32, y: i32, cell_size: f32) -> Cell {
    Cell(
        (x as f32 / cell_size).floor() as i32,
        (y as f32 / cell_size).floor() as i32,
    )
}

/// The snapshot header: `sequence` (field 1, varint) + the `delta`
/// flag (field 5; written only when true — a `false`/absent flag
/// means FULL, per `game.proto`).
pub(crate) fn write_snapshot_header(buf: &mut BytesMut, tick: u64, delta: bool) {
    buf.put_u8(0x08); // field 1 (sequence), varint
    encode_varint(tick, buf);
    if delta {
        buf.put_u8(0x28); // field 5 (delta), varint
        buf.put_u8(1);
    }
}

/// Encode `records` as `entities` entries (field 2, length-
/// delimited) — one pre-encoded piece, shareable by reference.
/// (Encoding straight into the buffer: no per-record allocation.)
pub(crate) fn encode_entity_records(records: &[(u64, i32, i32)]) -> Bytes {
    let mut out = BytesMut::new();
    for &(wire, x, y) in records {
        let rec = crate::game::EntityRecord { entity: wire, x, y };
        out.put_u8(0x12); // field 2 (entities), length-delimited
        encode_varint(rec.encoded_len() as u64, &mut out);
        rec.encode(&mut out)
            .expect("protobuf encode into an in-memory buffer failed");
    }
    out.freeze()
}

/// Encode `exits` as `removed` entries (field 3, varint) — the
/// entity-exit piece of a delta.
pub(crate) fn encode_entity_exits(exits: &[u64]) -> Bytes {
    let mut out = BytesMut::new();
    for &wire in exits {
        out.put_u8(0x18); // field 3 (removed), varint
        encode_varint(wire, &mut out);
    }
    out.freeze()
}

/// Encode one `cell_exits` entry (field 4, length-delimited) for
/// `cell` — the single record that makes the client forget a whole
/// cell.
pub(crate) fn encode_cell_exit(cell: Cell) -> Bytes {
    let msg = crate::game::CellExit {
        x: cell.0,
        y: cell.1,
    };
    let mut out = BytesMut::new();
    out.put_u8(0x22); // field 4 (cell_exits), length-delimited
    encode_varint(msg.encoded_len() as u64, &mut out);
    msg.encode(&mut out)
        .expect("protobuf encode into an in-memory buffer failed");
    out.freeze()
}

/// The per-tick classification of one cell (a pure function of the
/// cell's change list and its occupancy baseline — identical for every
/// group that sees the cell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellFrag {
    /// Empty now, empty before, and no change recorded this tick:
    /// nothing for any group.
    Silent,
    /// Non-empty before, empty now: the group's packet carries one
    /// `CellExit` record for it (the client forgets the whole cell in
    /// one record).
    Exited,
    /// Empty before, non-empty now: no baseline exists for the cell —
    /// its FULL records go into the group's packet (upserts in a delta
    /// packet; full content in a fresh group's packet).
    Appeared,
    /// Non-empty before and now, content changed: the cell's delta
    /// piece (its change list: exits + updates).
    Delta,
}

/// One cell's changes this tick — the delta's source of truth: the
/// change list *is* the diff, so no per-cell content comparison is ever
/// run. Built incrementally from the dirty set / the borrowed diff;
/// persistent map, cleared in place each tick.
#[derive(Default)]
pub(crate) struct CellChanges {
    /// Records whose wire content changed, or that newly occupy the cell
    /// (wire id, wire x, wire y). Encoded as `entities` upserts (field
    /// 2) — except for an appeared cell, whose group gets the cell's
    /// FULL piece instead (the upserts would be redundant: the client
    /// has no baseline for a cell that was empty).
    pub updates: Vec<(u64, i32, i32)>,
    /// Wire ids that left the cell (a cell-to-cell move or a despawn).
    /// Encoded as `removed` (field 3) — except when the whole cell
    /// exited, in which case one `CellExit` record supersedes them.
    pub exits: Vec<u64>,
    /// The cell was empty at the end of the last tick (set at roll
    /// time, order-independently — see [`CellBook::roll`]).
    pub appeared: bool,
    /// The cell is empty now (and was not empty then) — same evaluation.
    pub exited: bool,
}

/// The per-tick member-event counters of one touched cell: the
/// order-independent group-birth arithmetic reconstructs the before-tick
/// member count from `now − in + out`, so a same-tick exit+entry into
/// the same cell cannot fake a birth.
#[derive(Default)]
pub(crate) struct TouchInfo {
    /// Member entities that entered this cell this tick (joins into it,
    /// cell crossings into it).
    pub member_in: u32,
    /// Member entities that left it (crossings out, leavers).
    pub member_out: u32,
}
