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

/// The largest sequence a `uint64` holds decodes (a ten-byte varint).
#[test]
fn a_ten_byte_sequence_decodes() {
    let mut view = View::default();
    let got = view.apply_snapshot(&Frame::full(u64::MAX, &[(1, 0, 0)]).kit());
    assert_eq!(got.map(|s| s.sequence), Ok(u64::MAX));
}

/// Every malformed frame is an error, counted, and changes nothing —
/// including one whose envelope is fine but whose LAST record body is
/// not (the frame is decoded completely before the view changes), and
/// nothing of it leaks into the next frame.
#[test]
fn a_malformed_frame_is_an_error_and_changes_nothing() {
    // What a bad frame decodes before it fails: a removal and a record.
    let poison = Frame::delta(2, &[(9, 9, 9)]).removing(&[1]).kit();
    let mut bad_record = poison.clone();
    bad_record.extend_from_slice(&[0x12, 0x01, 0x08]); // record: truncated varint
    let mut bad_exit = poison.clone();
    bad_exit.extend_from_slice(&[0x22, 0x01, 0xFF]); // cell exit: truncated
    let cases: [(&str, Vec<u8>); 7] = [
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
        ("a record body", bad_record),
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
