//! The demo decode seam's hand-walked record against the generated
//! `EntityRecord` decoder: the same values for every body the demo's
//! codec writes, and a rejected body where the generated one rejects.

use super::*;
use gsb_demo::game::EntityRecord;

/// Every sample id × coordinate (proto3 zero omission, one- and
/// multi-byte varints, both signs, the `i32` extremes) decodes to what
/// the generated decoder reads; so does a body with an unknown field.
#[test]
fn the_hand_walked_record_matches_the_generated_decoder() {
    let decoder = DemoDecoder { cell_size: 20.0 };
    let coords = [0, 1, -1, 63, -64, 64, 20_000, -20_000, i32::MAX, i32::MIN];
    for entity in [0u64, 1, 127, 128, 1 << 40, u64::MAX] {
        for x in coords {
            for y in coords {
                let body = EntityRecord { entity, x, y }.encode_to_vec();
                let typed = EntityRecord::decode(&body[..]).expect("generated decodes");
                let got = decoder.record(&body).expect("hand decodes");
                assert_eq!(got, (typed.entity, (typed.x, typed.y)), "{entity} {x} {y}");
            }
        }
    }
    let mut body = EntityRecord {
        entity: 9,
        x: -3,
        y: 4,
    }
    .encode_to_vec();
    body.extend_from_slice(&[0x2A, 0x01, 0xFF]); // field 5, bytes
    assert_eq!(decoder.record(&body), Ok((9, (-3, 4))));
}

/// What the generated decoder rejects, the hand walk rejects.
#[test]
fn a_malformed_record_is_rejected() {
    let decoder = DemoDecoder { cell_size: 20.0 };
    for body in [
        &[0x08][..],       // entity: missing value
        &[0x0A, 0x00][..], // entity as bytes
        &[0x10, 0x80][..], // x: truncated varint
        &[0x1A, 0x05][..], // y as a truncated bytes field
    ] {
        assert!(EntityRecord::decode(body).is_err(), "{body:?}");
        assert!(decoder.record(body).is_err(), "{body:?}");
    }
}
