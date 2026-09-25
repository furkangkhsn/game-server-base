//! The record framing, byte for byte, in both framings.

use prost::Message;

use super::*;
use crate::testing::{FixCodec, PackedCodec, Record, WirePos, packed_body};

fn wires(n: u64) -> Vec<(u64, WirePos)> {
    (1..=n)
        .map(|i| {
            (
                i * 131,
                WirePos {
                    x: i as i32 * 3,
                    y: -(i as i32),
                },
            )
        })
        .collect()
}

/// The `entities` framing is untouched: one `0x12` entry per record,
/// the region writes nothing — for no record, one, and many.
#[test]
fn the_entities_framing_writes_one_entry_per_record_and_no_run() {
    for n in [0, 1, 40] {
        let records = wires(n);
        let mut out = BytesMut::from(&[0x08, 0x2A][..]);
        put_entity_records(&FixCodec, records.iter().map(|(id, w)| (*id, w)), &mut out);
        let mut want = vec![0x08, 0x2A];
        for &(entity, w) in &records {
            let body = Record {
                entity,
                x: w.x,
                y: w.y,
            }
            .encode_to_vec();
            want.extend_from_slice(&[0x12, body.len() as u8]);
            want.extend_from_slice(&body);
        }
        assert_eq!(&out[..], &want[..], "{n} records");
    }
}

/// The run: `0x32`, the run's minimal length (one byte below 128, more
/// above — the run moved right in place), then `id + body` per record;
/// no records → no field at all.
#[test]
fn the_run_is_one_field_with_a_minimal_length() {
    for n in [0, 1, 20, 40, 3_000] {
        let records = wires(n);
        let mut out = BytesMut::from(&[0x08, 0x2A][..]);
        put_entity_records(
            &PackedCodec,
            records.iter().map(|(id, w)| (*id, w)),
            &mut out,
        );
        let mut run = BytesMut::new();
        for (id, w) in &records {
            encode_varint(*id, &mut run);
            packed_body(w, &mut run);
        }
        let mut want = BytesMut::from(&[0x08, 0x2A][..]);
        if !run.is_empty() {
            want.put_u8(0x32);
            encode_varint(run.len() as u64, &mut want);
            want.extend_from_slice(&run);
        }
        assert_eq!(&out[..], &want[..], "{n} records ({} run bytes)", run.len());
    }
}

/// An empty region leaves no trace in either framing, wherever the
/// frame stands.
#[test]
fn an_empty_region_writes_nothing() {
    for run in [false, true] {
        let mut out = BytesMut::from(&[1, 2, 3][..]);
        Records::open(run, &mut out).close(&mut out);
        assert_eq!(&out[..], &[1, 2, 3], "run: {run}");
    }
}

/// A body another shard encoded is framed exactly as the typed record:
/// in both framings, the imported record is the owner's bytes.
#[test]
fn an_encoded_body_is_framed_like_the_typed_record() {
    for (id, w) in wires(5) {
        let (mut typed, mut spliced, mut body) =
            (BytesMut::new(), BytesMut::new(), BytesMut::new());
        put_entity_record(&FixCodec, id, &w, &mut typed);
        FixCodec.encode(id, &w, &mut body);
        put_entity_body::<FixCodec>(id, &body, &mut spliced);
        assert_eq!(typed, spliced, "entities");

        let (mut typed, mut spliced, mut body) =
            (BytesMut::new(), BytesMut::new(), BytesMut::new());
        put_entity_record(&PackedCodec, id, &w, &mut typed);
        PackedCodec.encode(id, &w, &mut body);
        put_entity_body::<PackedCodec>(id, &body, &mut spliced);
        assert_eq!(typed, spliced, "run");
    }
}
