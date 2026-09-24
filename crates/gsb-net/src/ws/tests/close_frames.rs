//! RFC 6455 §5.5.1 / §7.4 at the reader: what a client's close frame may
//! carry, and what the server echoes back.

use super::rig::*;
use super::*;

/// Close with `payload`; the reader must end the stream and queue the
/// echo (the status code alone) followed by the socket shutdown.
async fn assert_echoed(payload: &[u8], echo: &[u8]) {
    let mut rig = ReaderRig::new().await;
    rig.send(true, OP_CLOSE, payload).await;
    assert!(
        rig.next().await.is_none(),
        "close ends the stream: {payload:?}"
    );
    assert_eq!(
        rig.drain(),
        vec![Queued::Control(OP_CLOSE, echo.to_vec()), Queued::Shutdown],
        "payload {payload:?}"
    );
}

async fn failure_code_for(payload: &[u8]) -> u16 {
    let mut rig = ReaderRig::new().await;
    rig.send(true, OP_CLOSE, payload).await;
    rig.failure_code().await
}

#[tokio::test]
async fn an_empty_close_is_echoed_empty() {
    assert_echoed(&[], &[]).await;
}

#[tokio::test]
async fn a_one_byte_close_payload_fails_with_1002() {
    assert_eq!(failure_code_for(&[0x03]).await, 1002);
}

/// §7.4.1 defines 1000-1003 and 1007-1011; IANA has since registered
/// 1012-1014; 3000-4999 belong to libraries and applications. Each is
/// echoed (the reason text is not).
#[tokio::test]
async fn every_sendable_close_code_is_echoed() {
    let codes = (1000u16..=1003)
        .chain(1007..=1014)
        .chain([3000, 3999, 4000, 4999]);
    for code in codes {
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice(b"bye");
        assert_echoed(&payload, &code.to_be_bytes()).await;
    }
}

/// 0-999 are unused; 1004 is reserved; 1005, 1006 and 1015 are reserved
/// for APIs and MUST NOT appear in a close frame; 1016-2999 are
/// unassigned; nothing past 4999 is defined. Each is a protocol error.
#[tokio::test]
async fn an_unsendable_close_code_fails_with_1002() {
    for code in [
        0, 999, 1004, 1005, 1006, 1015, 1016, 1100, 2000, 2999, 5000, 65_535,
    ] {
        assert_eq!(
            failure_code_for(&u16::to_be_bytes(code)).await,
            1002,
            "code {code}"
        );
    }
}

/// §8.1: a close reason is UTF-8; one that is not fails with 1007. The
/// bytes are Autobahn 7.5.1's (a lone surrogate encoded mid-reason).
#[tokio::test]
async fn a_close_reason_that_is_not_utf8_fails_with_1007() {
    let mut payload = 1000u16.to_be_bytes().to_vec();
    payload.extend_from_slice(&[
        0xce, 0xba, 0xe1, 0xbd, 0xb9, 0xcf, 0x83, 0xce, 0xbc, 0xce, 0xb5, 0xed, 0xa0, 0x80, 0x65,
        0x64, 0x69, 0x74, 0x65, 0x64,
    ]);
    assert_eq!(failure_code_for(&payload).await, 1007);
}

/// The longest legal reason (123 bytes, multi-byte UTF-8 included) is
/// accepted; the code is echoed.
#[tokio::test]
async fn a_maximal_utf8_reason_is_accepted() {
    let mut payload = 1000u16.to_be_bytes().to_vec();
    let reason = "é".repeat(61) + "x"; // 61 × 2 + 1 = 123 bytes
    payload.extend_from_slice(reason.as_bytes());
    assert_eq!(payload.len(), MAX_CONTROL_PAYLOAD);
    assert_echoed(&payload, &1000u16.to_be_bytes()).await;
}
