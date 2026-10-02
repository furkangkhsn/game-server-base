//! The teardown close's code for every way a session ends (BACKLOG B30):
//! the whole mapping, pinned, and each code a legal one to send.

use gsb_core::conn::{ServerClose, SessionEnd};

use super::*;

/// Every reason, by the code it closes with.
fn expected(verdict: ServerClose) -> u16 {
    match verdict {
        ServerClose::RoomGone | ServerClose::OutboundDead => 1001,
        ServerClose::ConnCap | ServerClose::UnauthCap => 1013,
        ServerClose::IdleTimeout
        | ServerClose::WriteStall
        | ServerClose::RelDead
        | ServerClose::ViolationBudget
        | ServerClose::PreauthBudget
        | ServerClose::StreamRejected
        | ServerClose::Superseded
        | ServerClose::IdleInput
        | ServerClose::Kicked => 1008,
    }
}

#[test]
fn the_stop_and_the_client_s_end_keep_1001() {
    assert_eq!(close_code(Some(SessionEnd::Stopped)), 1001);
    assert_eq!(close_code(Some(SessionEnd::Client)), 1001);
    assert_eq!(close_code(None), 1001, "an actor that never told");
}

#[test]
fn each_verdict_closes_with_its_code() {
    for verdict in ServerClose::ALL {
        assert_eq!(
            close_code(Some(SessionEnd::Verdict(verdict))),
            expected(verdict),
            "{verdict:?}"
        );
    }
}

/// RFC 6455 §7.4.1/§7.4.2: 1001 and 1008 are defined there, 1013 is in
/// the IANA registry; none is one of the codes an endpoint must not send
/// in a close frame (1005, 1006, 1015) or a reserved one (1004).
#[test]
fn every_code_is_sendable() {
    for code in [
        CLOSE_GOING_AWAY,
        CLOSE_POLICY_VIOLATION,
        CLOSE_TRY_AGAIN_LATER,
    ] {
        assert!((1000..=1014).contains(&code), "{code}");
        assert!(![1004, 1005, 1006, 1015].contains(&code), "{code}");
    }
    assert_eq!(
        (
            CLOSE_GOING_AWAY,
            CLOSE_POLICY_VIOLATION,
            CLOSE_TRY_AGAIN_LATER
        ),
        (1001, 1008, 1013)
    );
}
