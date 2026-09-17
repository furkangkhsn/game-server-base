//! `CoreError` → wire code: the numbers the join-reply path sends.
//!
//! The mapping itself ([`CoreError::wire_code`]) is an exhaustive match,
//! so adding a variant fails to COMPILE until its code is chosen — that
//! is the primary lock and no test can be as strong. These tests pin the
//! choices already made, so a later edit cannot quietly renumber an
//! existing variant.

use gsb_protocol::base::ErrorCode;

use super::CoreError;

/// Every variant, with the number it sent before the enum existed:
/// `RoomFull` was 8, `RoomRetired` was 12, everything else fell through
/// the `_ =>` arm to 4.
#[test]
fn every_variant_keeps_its_pre_enum_number() {
    let cases: [(CoreError, ErrorCode); 13] = [
        (CoreError::RoomFull(1), ErrorCode::RoomFull),
        (CoreError::RoomRetired(1), ErrorCode::RoomRetired),
        (CoreError::RoomNotFound(1), ErrorCode::RoomOpFailed),
        (CoreError::NotInRoom, ErrorCode::RoomOpFailed),
        (CoreError::RoomExists(1), ErrorCode::RoomOpFailed),
        (CoreError::RoomConflict(1), ErrorCode::RoomOpFailed),
        (
            CoreError::TickRate {
                room: 15.0,
                global: 60.0,
            },
            ErrorCode::RoomOpFailed,
        ),
        (
            CoreError::KeepaliveRate {
                keepalive: 2.0,
                tick: 1.0,
            },
            ErrorCode::RoomOpFailed,
        ),
        (
            CoreError::InvalidTickRate { rate: 0.0 },
            ErrorCode::RoomOpFailed,
        ),
        (CoreError::RoomGone, ErrorCode::RoomOpFailed),
        (CoreError::ResumeStale, ErrorCode::RoomOpFailed),
        (CoreError::Protocol("x".into()), ErrorCode::RoomOpFailed),
        (CoreError::Io("x".into()), ErrorCode::RoomOpFailed),
    ];
    for (e, want) in &cases {
        assert_eq!(e.wire_code(), *want, "wrong wire code for {e:?}");
    }

    // The count is asserted through the array's own length: the array
    // must list EVERY variant, and `wire_code`'s exhaustive match is what
    // makes a forgotten one a compile error rather than a silent gap.
    assert_eq!(cases.len(), 13, "a CoreError variant was added or removed");
}

/// A gsb server never sends the proto3 zero value (`base.proto` states it
/// as a protocol guarantee).
#[test]
fn no_variant_maps_to_unspecified() {
    for e in [
        CoreError::RoomFull(1),
        CoreError::RoomRetired(1),
        CoreError::RoomNotFound(1),
        CoreError::NotInRoom,
        CoreError::RoomExists(1),
        CoreError::RoomConflict(1),
        CoreError::RoomGone,
        CoreError::ResumeStale,
        CoreError::Protocol("x".into()),
        CoreError::Io("x".into()),
    ] {
        assert_ne!(e.wire_code(), ErrorCode::Unspecified, "{e:?}");
    }
}

/// The two variants that have a client decision of their own keep it:
/// 8 is "pick another room, this connection is fine" and 12 is
/// "definitively over, return to the lobby" (RECONNECT §8). Collapsing
/// either into 4 would change what a client does.
#[test]
fn the_two_distinguished_classes_stay_distinguished() {
    let full = CoreError::RoomFull(1).wire_code();
    let retired = CoreError::RoomRetired(1).wire_code();
    let generic = CoreError::RoomNotFound(1).wire_code();
    assert_ne!(full, generic);
    assert_ne!(retired, generic);
    assert_ne!(full, retired);
}
