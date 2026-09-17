//! Tests that lock the `ERROR` numbering.
//!
//! Before the protocol-hardening round the numbering lived in a comment
//! table above `message Error` and was re-typed by hand in `gsb-core`
//! (server) and in the loadgen / example client. It is now the generated
//! [`crate::base::ErrorCode`], and these tests are what stop it drifting:
//! the numbers are pinned one by one, `ProtoError`'s mapping is pinned
//! per variant, and nothing in either mapping may return the proto3 zero
//! value.

use crate::ProtoError;
use crate::base::ErrorCode;

/// Every code, pinned to the number the pre-enum comment table gave it
/// (`base.proto` before this round; see `docs/CHANGELOG.md`). A change
/// here is a WIRE change and must be a deliberate one.
#[test]
fn numbers_match_the_pre_enum_comment_table() {
    assert_eq!(ErrorCode::Unspecified as i32, 0);
    assert_eq!(ErrorCode::UnknownOpcode as i32, 1);
    assert_eq!(ErrorCode::Decode as i32, 2);
    assert_eq!(ErrorCode::Auth as i32, 3);
    assert_eq!(ErrorCode::RoomOpFailed as i32, 4);
    assert_eq!(ErrorCode::RoomDestroyed as i32, 5);
    assert_eq!(ErrorCode::NotInRoom as i32, 6);
    assert_eq!(ErrorCode::Other as i32, 7);
    assert_eq!(ErrorCode::RoomFull as i32, 8);
    assert_eq!(ErrorCode::ServerClosed as i32, 9);
    assert_eq!(ErrorCode::TicketInvalid as i32, 10);
    assert_eq!(ErrorCode::RoomMismatch as i32, 11);
    assert_eq!(ErrorCode::RoomRetired as i32, 12);
}

/// The enum is exactly 0..=12 with no gaps — so a new code cannot be
/// slipped in at a number the old comment table already spent, and this
/// test fails the moment one is appended (forcing the author to extend
/// the pinned list above and the docs beside it).
#[test]
fn the_code_space_is_exactly_zero_through_twelve() {
    let all: Vec<i32> = (0..=12).collect();
    let known: Vec<i32> = (0..=64)
        .filter(|n| ErrorCode::try_from(*n).is_ok())
        .collect();
    assert_eq!(known, all, "the ErrorCode space changed");
}

/// `ProtoError`'s mapping, variant by variant. The mapping itself is an
/// exhaustive match, so a NEW variant fails to compile rather than
/// reaching this test; this pins the choices already made.
#[test]
fn proto_error_maps_to_the_documented_codes() {
    let cases: [(ProtoError, ErrorCode); 8] = [
        (ProtoError::UnknownOpcode(7), ErrorCode::UnknownOpcode),
        (
            ProtoError::Decode {
                op: 1,
                reason: "x".into(),
            },
            ErrorCode::Decode,
        ),
        (ProtoError::NotAuthenticated, ErrorCode::Auth),
        (ProtoError::AlreadyAuthenticated, ErrorCode::Auth),
        (ProtoError::RoomNotFound(1), ErrorCode::RoomOpFailed),
        (ProtoError::NotInRoom, ErrorCode::NotInRoom),
        (ProtoError::MalformedFrame(1), ErrorCode::Other),
        (ProtoError::Other("x".into()), ErrorCode::Other),
    ];
    for (e, want) in cases {
        assert_eq!(e.wire_code(), want, "wrong code for {e:?}");
    }
}

/// A gsb server never sends the proto3 zero value — `base.proto` states
/// it as a protocol guarantee, so the mapping must never produce it.
#[test]
fn no_mapping_is_unspecified() {
    for e in [
        ProtoError::UnknownOpcode(7),
        ProtoError::Decode {
            op: 1,
            reason: "x".into(),
        },
        ProtoError::NotAuthenticated,
        ProtoError::AlreadyAuthenticated,
        ProtoError::RoomNotFound(1),
        ProtoError::NotInRoom,
        ProtoError::MalformedFrame(1),
        ProtoError::Other("x".into()),
    ] {
        assert_ne!(e.wire_code(), ErrorCode::Unspecified, "{e:?}");
    }
}

/// The `code` field stays a varint at tag 1, byte-identical to the
/// `uint32` it replaced over the whole 1..=12 range — the enum is a
/// source-level change, not a wire change.
#[test]
fn the_code_field_encodes_exactly_as_the_old_uint32_did() {
    use prost::Message;

    for n in 0..=12i32 {
        let code = ErrorCode::try_from(n).expect("in range");
        let got = crate::base::Error::new(code, "").encode_to_vec();
        if n == 0 {
            // proto3 omits a default-valued scalar; `uint32 code = 1`
            // did exactly the same for 0.
            assert!(got.is_empty(), "code 0 must encode to nothing");
        } else {
            // tag 1, wire type 0 (varint) = 0x08, then the varint. Every
            // value here is < 128, so one byte.
            assert_eq!(got, vec![0x08, n as u8], "code {n}");
        }
    }
}

/// The convenience constructor is the only server-side way to build an
/// error, and it round-trips through the open enum accessor.
#[test]
fn error_new_round_trips_through_the_enum_accessor() {
    let e = crate::base::Error::new(ErrorCode::RoomRetired, "gone");
    assert_eq!(e.code, 12);
    assert_eq!(e.code(), ErrorCode::RoomRetired);
    assert_eq!(e.message, "gone");
}

/// The open-enum contract a client depends on: an unrecognised number
/// survives decoding (it stays in the raw `i32` field) and only the
/// typed accessor collapses it to the zero value. That is what makes
/// base.proto's forward-compatibility rule implementable — a client can
/// log the real number while treating the class as OTHER.
#[test]
fn an_unknown_code_survives_decoding() {
    use prost::Message;

    let wire = [0x08, 99]; // code = 99, a number no build knows
    let decoded = crate::base::Error::decode(&wire[..]).expect("decode");
    assert_eq!(decoded.code, 99, "the raw number must be preserved");
    assert_eq!(
        decoded.code(),
        ErrorCode::Unspecified,
        "the typed accessor reports unknown as the zero value"
    );
}
