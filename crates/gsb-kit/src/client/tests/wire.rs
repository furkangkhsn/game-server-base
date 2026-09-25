//! The envelope walk: what a protobuf parser accepts is accepted, what
//! it rejects is an error that leaves the view as it was.

use super::*;

/// Unknown fields of every wire type are skipped, anywhere in the frame
/// (a newer envelope or a mirror's extra field).
#[test]
fn unknown_fields_are_skipped() {
    let base = Frame::delta(2, &[(1, 3, 3)]).removing(&[2]).kit();
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0x30, 0x96, 0x01]); // 6: varint 150
    frame.extend_from_slice(&[0x39, 1, 2, 3, 4, 5, 6, 7, 8]); // 7: fixed64
    frame.extend_from_slice(&base);
    frame.extend_from_slice(&[0x42, 0x02, 0xAA, 0xBB]); // 8: bytes
    frame.extend_from_slice(&[0x4D, 1, 2, 3, 4]); // 9: fixed32
    let mut view = View::default();
    view.apply_snapshot(&Frame::full(1, &[(1, 0, 0), (2, 0, 0)]).kit())
        .expect("decodes");
    let got = view.apply_snapshot(&frame).expect("unknown fields skipped");
    assert_eq!(got.apply, Apply::Delta);
    assert_eq!(sorted(&view), [(1, 3, 3)]);
}

/// Varints of every length decode — across each 7-bit boundary, up to
/// the largest `uint64` (a ten-byte varint).
#[test]
fn varints_of_every_length_decode() {
    let mut edges = vec![0u64, 1, u64::MAX];
    for bits in (7..64).step_by(7) {
        edges.extend([(1u64 << bits) - 1, 1u64 << bits]);
    }
    for seq in edges {
        let mut view = View::default();
        let frame = Frame::full(seq, &[(seq, 0, 0)]);
        for bytes in [frame.kit(), frame.generated()] {
            let got = view.apply_snapshot(&bytes).expect("decodes");
            assert_eq!(got.sequence, seq);
            assert!(view.contains(seq), "{seq}");
        }
    }
}

/// A malformed envelope or cell exit is an error, counted, and changes
/// nothing — the whole envelope is walked before the view changes — and
/// nothing of it leaks into the next frame.
#[test]
fn a_malformed_envelope_is_an_error_and_changes_nothing() {
    // What a bad frame holds before it fails: a removal and a record.
    let poison = Frame::delta(2, &[(9, 9, 9)]).removing(&[1]).kit();
    let mut bad_exit = poison.clone();
    bad_exit.extend_from_slice(&[0x22, 0x01, 0xFF]); // cell exit: truncated
    let cases: [(&str, Vec<u8>); 6] = [
        ("truncated body", poison[..poison.len() - 1].to_vec()),
        ("truncated key", vec![0x08]),
        (
            "eleven-byte varint",
            vec![
                0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02,
            ],
        ),
        ("field number 0", vec![0x00, 0x01]),
        ("sequence as bytes", vec![0x0A, 0x00]),
        ("a cell-exit body", bad_exit),
    ];
    let good = Frame::delta(2, &[(3, 3, 3)]).kit();
    for (what, frame) in cases {
        let mut view = View::default();
        view.apply_snapshot(&Frame::full(1, &[(1, 0, 0)]).kit())
            .expect("decodes");
        assert!(view.apply_snapshot(&frame).is_err(), "{what}");
        assert_eq!(sorted(&view), [(1, 0, 0)], "{what}: view untouched");
        assert_eq!(view.last_sequence(), Some(1), "{what}");
        assert_eq!(view.counters().errors, 1, "{what}");
        // The next good frame applies alone: nothing of the bad one.
        let next = view.apply_snapshot(&good).expect("decodes");
        assert_eq!(next.apply, Apply::Delta, "{what}");
        assert_eq!(sorted(&view), [(1, 0, 0), (3, 3, 3)], "{what}: no leak");
    }
}

/// A record body the game's decoder rejects is found while the view is
/// already changing: the view is left EMPTY and WITHOUT a baseline —
/// never half a frame (here: the removal and the first record applied,
/// the last record not) — so deltas drop until the next full, whether
/// the bad frame was a group delta, a group full or the private full.
#[test]
fn an_undecodable_record_leaves_the_view_without_a_baseline() {
    let bad_record = |f: Frame| {
        let mut bytes = f.kit();
        bytes.extend_from_slice(&[0x12, 0x01, 0x08]); // record: truncated
        bytes
    };
    // The same bad full as the one-shot private arm (`Private.snapshot`,
    // field 2, length-delimited).
    let snapshot = bad_record(Frame::full(2, &[(9, 9, 9)]));
    let mut private_bad = vec![0x12];
    prost::encoding::encode_varint(snapshot.len() as u64, &mut private_bad);
    private_bad.extend_from_slice(&snapshot);
    for (what, frame, private) in [
        (
            "delta",
            bad_record(Frame::delta(2, &[(9, 9, 9)]).removing(&[1])),
            false,
        ),
        ("full", bad_record(Frame::full(2, &[(9, 9, 9)])), false),
        ("private full", private_bad, true),
    ] {
        let mut view = View::default();
        view.apply_snapshot(&Frame::full(1, &[(1, 0, 0), (2, 0, 0)]).kit())
            .expect("decodes");
        let got = if private {
            view.apply_private(&frame).map(|_| ())
        } else {
            view.apply_snapshot(&frame).map(|_| ())
        };
        assert!(matches!(got, Err(ClientError::Body(_))), "{what}: {got:?}");
        assert!(view.is_empty() && !view.has_baseline(), "{what}");
        assert_eq!(view.counters().errors, 1, "{what}");
        let next = view.apply_snapshot(&Frame::delta(3, &[(4, 4, 4)]).kit());
        assert_eq!(next.map(|s| s.apply), Ok(Apply::NoBaseline), "{what}");
        let full = view.apply_snapshot(&Frame::full(4, &[(5, 5, 5)]).kit());
        assert_eq!(full.map(|s| s.apply), Ok(Apply::Full), "{what}");
        assert_eq!(sorted(&view), [(5, 5, 5)], "{what}");
    }
}

/// `Private.payload` is a oneof: when two arms reach the wire (two
/// concatenated messages merge), the LAST one is the payload.
#[test]
fn the_last_private_arm_wins() {
    let ack = proto::Private {
        payload: Some(proto::private::Payload::Ack(proto::InputAck {
            processed_up_to: 3,
        })),
        ..Default::default()
    }
    .encode_to_vec();
    let full = Frame::full(5, &[(1, 0, 0)]).private();
    let mut view = View::default();
    let got = view.apply_private(&[full.clone(), ack.clone()].concat());
    assert_eq!(got, Ok(PrivateEvent::Ack(3)));
    assert!(
        !view.has_baseline(),
        "the earlier snapshot arm is not applied"
    );
    let got = view.apply_private(&[ack, full].concat());
    assert_eq!(got, Ok(PrivateEvent::Full { sequence: 5 }));
    assert_eq!(sorted(&view), [(1, 0, 0)]);
}

/// A group (wire type 3), which proto3 cannot declare, is rejected; a
/// malformed `Private` is an error too.
#[test]
fn groups_and_malformed_private_frames_are_errors() {
    let mut view = View::default();
    assert!(view.apply_snapshot(&[0x0B, 0x0C]).is_err());
    assert!(
        view.apply_private(&[0x0A, 0x05, 0x08]).is_err(),
        "ack truncated"
    );
    assert!(
        view.apply_private(&[0x10, 0x01]).is_err(),
        "snapshot as varint"
    );
    assert_eq!(view.counters().errors, 3);
    assert!(!view.has_baseline());
}

/// The public walker reads every wire type's value, and `sint32` is
/// protobuf's zigzag (checked against the generated decoder).
#[test]
fn the_walker_reads_every_wire_type_and_sint32_is_zigzag() {
    use crate::client::wire::{Fields, Malformed, Value, sint32};
    let frame = [
        0x08, 0x96, 0x01, // 1: varint 150
        0x11, 1, 0, 0, 0, 0, 0, 0, 0x80, // 2: fixed64
        0x1A, 0x02, 0xAA, 0xBB, // 3: bytes
        0x25, 4, 3, 2, 1, // 4: fixed32
    ];
    let fields: Result<Vec<_>, Malformed> = Fields::new(&frame).collect();
    assert_eq!(
        fields,
        Ok(vec![
            (1, Value::Varint(150)),
            (2, Value::Fixed64(0x8000_0000_0000_0001)),
            (3, Value::Len(&[0xAA, 0xBB][..])),
            (4, Value::Fixed32(0x0102_0304)),
        ])
    );
    for x in [0, 1, -1, 63, -64, i32::MAX, i32::MIN] {
        let body = CellExit { x, y: 0 }.encode_to_vec();
        let got: Vec<i32> = Fields::new(&body)
            .map(|f| match f {
                Ok((1, Value::Varint(v))) => sint32(v),
                other => panic!("{other:?}"),
            })
            .collect();
        let want = if x == 0 { vec![] } else { vec![x] }; // proto3 omits 0
        assert_eq!(got, want, "{x}");
    }
    // A `sint32` varint wider than 32 bits is truncated first, as the
    // generated decoder does.
    let wide = [0x08, 0x82, 0x80, 0x80, 0x80, 0x10]; // x: 0x1_0000_0002
    assert_eq!(CellExit::decode(&wide[..]).map(|c| c.x), Ok(1));
    assert_eq!(sint32(0x1_0000_0002), 1);
    // A walk stops at the first malformed field (and yields nothing more).
    let mut bad = Fields::new(&[0x08, 0x01, 0x0B, 0x08, 0x02]);
    assert_eq!(bad.next(), Some(Ok((1, Value::Varint(1)))));
    assert!(matches!(bad.next(), Some(Err(_))));
    assert_eq!(bad.next(), None);
}
