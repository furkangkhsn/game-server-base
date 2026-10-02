//! Stateless reset, client side (B5b): a datagram of a reset's shape
//! whose last 16 bytes are the session's token (message 2's) ends the
//! session at once, counted under its own name only; a wrong token, or
//! the right bytes in a datagram no reset can be, is a refused record
//! like any other and the session goes on.

use super::seal::{seal, session_pair};
use super::*;
use crate::seal::{RESET_LEN_MAX, ResetToken, reset_datagram};

/// A sealed client holding `token`, its server's sealer for genuine
/// records.
async fn sealed(token: ResetToken) -> (UdpClient, UdpSocket, crate::seal::Sealer) {
    let (ss, _, cs, co) = session_pair();
    let config = UdpClientConfig {
        server_key: Some([1; 32]),
        ..UdpClientConfig::default()
    };
    let (mut c, sink) = detached_with(config).await;
    c.seal.install(cs, co, token);
    (c, sink, ss)
}

#[tokio::test]
async fn a_reset_with_the_sessions_token_ends_the_session_at_once() {
    let token = ResetToken::from_bytes([0x5A; 16]);
    let (mut c, _sink, mut ss) = sealed(token).await;
    // A control frame outstanding: never delivered now.
    c.send_frame(8, Bytes::from_static(b"hb")).await.unwrap();
    assert!(c.process_datagram(&seal(
        &mut ss,
        &encode_raw(&FrameBody::new(1000, Bytes::new()))
    )));
    let reset = reset_datagram(&token, 40, &[0xA7; RESET_LEN_MAX]).unwrap();
    assert!(!c.process_datagram(&reset));
    assert!(!c.is_established(), "over at once, no 5 s wait");
    let s = &c.stats;
    assert_eq!(
        (
            s.stateless_resets_received,
            s.stateless_resets_invalid,
            s.seal_forged
        ),
        (1, 0, 0),
        "counted under its own name only"
    );
    assert_eq!(s.gave_up, 1, "the outstanding frame is never delivered");
}

#[tokio::test]
async fn a_wrong_token_or_a_non_reset_shape_is_a_refused_record() {
    let token = ResetToken::from_bytes([0x5A; 16]);
    let (mut c, _sink, _ss) = sealed(token).await;
    // Another session's token (or a restarted server whose key changed).
    let other = ResetToken::from_bytes([0x5B; 16]);
    let wrong = reset_datagram(&other, 40, &[0xA7; RESET_LEN_MAX]).unwrap();
    assert!(!c.process_datagram(&wrong));
    assert!(c.is_established());
    assert_eq!(
        (c.stats.stateless_resets_invalid, c.stats.seal_forged),
        (1, 1)
    );
    // The right token ending a datagram longer than any reset: a record
    // that failed to open, nothing more.
    let right = reset_datagram(&token, 40, &[0xA7; RESET_LEN_MAX]).unwrap();
    let long = [&[0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0][..], &right].concat();
    assert!(!c.process_datagram(&long));
    // The right token after a plaintext kind byte: not SEALED, not read.
    let mut plain = right.clone();
    plain[0] = KIND_RAW;
    assert!(!c.process_datagram(&plain));
    assert!(c.is_established());
    let s = &c.stats;
    assert_eq!(
        (
            s.stateless_resets_received,
            s.stateless_resets_invalid,
            s.seal_forged
        ),
        (0, 1, 2)
    );
    assert_eq!(s.unsealed_dropped, 1);
}
