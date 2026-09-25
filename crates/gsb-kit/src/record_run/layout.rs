//! What a frame IS, byte for byte, in each framing: a snapshot is taken
//! apart field by field, checked to follow the kit's fixed layout, and
//! rebuilt from its parts with minimal encodings — the rebuild must be
//! the very bytes the room wrote.
//!
//! - `entities` framing (a game that did not opt in): `sequence` (field
//!   1, omitted when 0), `delta` (field 5, only when true), every
//!   `removed` id (field 3, unpacked), every `cell_exits` entry (field
//!   4), every record as its own `entities` entry (field 2) — and NO
//!   field 6. The layout every kit room wrote before A31.
//! - record-run framing: the same header, `removed` and `cell_exits`,
//!   then at most ONE `records` field (6, never empty), no field 2; the
//!   run is `id varint + packed body` per record, back to back.

use bytes::BytesMut;
use prost::Message;
use prost::encoding::varint::encode_varint;

use crate::client::ClientView;
use crate::client::wire::{Fields, Value, varint};
use crate::testing::{Dec, Record, WirePos, packed_body, read_packed};

/// One snapshot, taken apart (records and removals sorted: a room's
/// iteration order is its own).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Parts {
    pub(super) sequence: u64,
    pub(super) delta: bool,
    pub(super) removed: Vec<u64>,
    pub(super) exits: Vec<Vec<u8>>,
    pub(super) records: Vec<(u64, i32, i32)>,
    /// The record run's length (0: none, or the `entities` framing).
    pub(super) run_len: usize,
}

/// Take `frame` apart, asserting it is exactly the `run` (or entities)
/// framing's layout.
pub(super) fn parts(frame: &[u8], run: bool) -> Parts {
    let mut p = Parts::default();
    let mut stage = 0; // 1 header seq, 2 delta, 3 removed, 4 exits, 5 records
    let mut records_in_wire_order = Vec::new();
    for field in Fields::new(frame) {
        let (number, value) = field.expect("a well-formed frame");
        let at = match (number, value) {
            (1, Value::Varint(v)) => {
                p.sequence = v;
                1
            }
            (5, Value::Varint(1)) => {
                p.delta = true;
                2
            }
            (3, Value::Varint(id)) => {
                p.removed.push(id);
                3
            }
            (4, Value::Len(body)) => {
                p.exits.push(body.to_vec());
                4
            }
            (2, Value::Len(body)) if !run => {
                let r = Record::decode(body).expect("an entities record");
                records_in_wire_order.push((r.entity, r.x, r.y));
                5
            }
            (6, Value::Len(mut body)) if run => {
                assert!(p.run_len == 0 && !body.is_empty(), "one run, never empty");
                p.run_len = body.len();
                while !body.is_empty() {
                    let id = varint(&mut body).expect("an id");
                    let w = read_packed(&mut body).expect("a packed body");
                    records_in_wire_order.push((id, w.x, w.y));
                }
                5
            }
            other => panic!("field {other:?} out of place in a run={run} frame"),
        };
        assert!(
            at > stage || (at == stage && at >= 3),
            "field order: {at} after {stage}"
        );
        stage = at;
    }
    assert_eq!(
        rebuild(&p, &records_in_wire_order, run),
        frame,
        "the frame is exactly its parts, minimally encoded"
    );
    p.records = records_in_wire_order;
    p.records.sort_unstable();
    p.removed.sort_unstable();
    p.exits.sort_unstable();
    p
}

/// The frame of `p` (records in their wire order) in the given framing.
fn rebuild(p: &Parts, records: &[(u64, i32, i32)], run: bool) -> Vec<u8> {
    let mut out = BytesMut::new();
    if p.sequence != 0 {
        out.extend_from_slice(&[0x08]);
        encode_varint(p.sequence, &mut out);
    }
    if p.delta {
        out.extend_from_slice(&[0x28, 0x01]);
    }
    for &id in &p.removed {
        out.extend_from_slice(&[0x18]);
        encode_varint(id, &mut out);
    }
    for exit in &p.exits {
        delimited(&mut out, 0x22, exit);
    }
    if run {
        let mut body = BytesMut::new();
        for &(id, x, y) in records {
            encode_varint(id, &mut body);
            packed_body(&WirePos { x, y }, &mut body);
        }
        if !body.is_empty() {
            delimited(&mut out, 0x32, &body);
        }
    } else {
        for &(entity, x, y) in records {
            delimited(&mut out, 0x12, &Record { entity, x, y }.encode_to_vec());
        }
    }
    out.to_vec()
}

fn delimited(out: &mut BytesMut, tag: u8, body: &[u8]) {
    out.extend_from_slice(&[tag]);
    encode_varint(body.len() as u64, out);
    out.extend_from_slice(body);
}

/// The one-shot full a `Private` frame carries, if any (its arm, field
/// 2); every other field is left to the view.
pub(super) fn private_snapshot(frame: &[u8]) -> Option<&[u8]> {
    let mut found = None;
    for field in Fields::new(frame) {
        if let (2, Value::Len(body)) = field.expect("a well-formed private frame") {
            found = Some(body);
        }
    }
    found
}

/// Apply one frame (a group snapshot, or a `Private` frame) to `view`
/// under the kit's client rules, recording its snapshot's parts.
pub(super) fn take<const RUN: bool>(
    view: &mut ClientView<Dec<RUN>>,
    frames: &mut Vec<(bool, Parts)>,
    snapshot: bool,
    payload: &[u8],
) {
    if snapshot {
        frames.push((false, parts(payload, RUN)));
        view.apply_snapshot(payload).expect("a group frame applies");
    } else {
        if let Some(full) = private_snapshot(payload) {
            frames.push((true, parts(full, RUN)));
        }
        view.apply_private(payload)
            .expect("a private frame applies");
    }
}
