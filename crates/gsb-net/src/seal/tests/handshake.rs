//! The handshake wrapper: round trip, every refusal, recovery from a
//! forged accept, the reset token.

use super::*;

fn server() -> StaticKey {
    StaticKey::generate().unwrap()
}

fn accept(cid: u64) -> Accept {
    Accept {
        cid,
        reset_token: ResetKey::from_bytes([1; 32]).token(cid),
    }
}

#[test]
fn round_trip_carries_payloads_and_agrees_on_the_hash() {
    let key = server();
    let mut init = Initiator::new(&key.public(), b"nonce+cookie", b"caps").unwrap();
    assert_eq!(init.msg1().len(), MSG1_LEN_MIN + 4);
    let resp = Msg1::parse(init.msg1())
        .unwrap()
        .cookie_verified(&key, b"nonce+cookie", &accept(0xc1d))
        .unwrap();
    assert_eq!(resp.payload, b"caps");
    assert_eq!(resp.msg2.len(), MSG2_LEN);
    let Ok((got, client)) = init.finish(&resp.msg2) else {
        panic!("msg2")
    };
    assert_eq!(got, accept(0xc1d));
    assert_eq!(client.handshake_hash(), resp.session.handshake_hash());

    let (mut cs, mut co) = client.into_halves();
    let (mut ss, mut so) = resp.session.into_halves();
    let mut d = Vec::new();
    cs.seal(b"auth ticket", &mut d).unwrap();
    assert_eq!(
        Header::decode(&d, Direction::ClientToServer).unwrap().cid,
        Some(0xc1d)
    );
    assert_eq!(so.open(&d).unwrap().plaintext, b"auth ticket");
    d.clear();
    ss.seal(b"welcome", &mut d).unwrap();
    assert_eq!(co.open(&d).unwrap().plaintext, b"welcome");
}

#[test]
fn sessions_are_unique_even_for_the_same_inputs() {
    let (a, b) = (pair(1), pair(1));
    let (mut ca, _, _, _, _) = a;
    let (_, _, _, mut sob, _) = b;
    let mut d = Vec::new();
    ca.seal(b"x", &mut d).unwrap();
    assert_eq!(
        sob.open(&d),
        Err(Refusal::Forged),
        "fresh ephemeral keys per session"
    );
}

#[test]
fn a_client_pinning_another_key_is_refused_by_the_server() {
    let (real, other) = (server(), server());
    let init = Initiator::new(&other.public(), b"ctx", b"").unwrap();
    let r = Msg1::parse(init.msg1())
        .unwrap()
        .cookie_verified(&real, b"ctx", &accept(1));
    assert!(matches!(r, Err(HandshakeError::Decrypt)));
}

#[test]
fn a_context_mismatch_is_refused() {
    let key = server();
    let init = Initiator::new(&key.public(), b"cookie A", b"").unwrap();
    let r = Msg1::parse(init.msg1())
        .unwrap()
        .cookie_verified(&key, b"cookie B", &accept(1));
    assert!(matches!(r, Err(HandshakeError::Decrypt)));
}

#[test]
fn message_lengths_are_checked_before_any_dh() {
    assert!(matches!(
        Msg1::parse(&[0; MSG1_LEN_MIN - 1]),
        Err(HandshakeError::Malformed)
    ));
    assert!(matches!(
        Msg1::parse(&[0; MSG1_LEN_MAX + 1]),
        Err(HandshakeError::Malformed)
    ));
    assert!(Msg1::parse(&[0; MSG1_LEN_MIN]).is_ok());
    assert!(Msg1::parse(&[0; MSG1_LEN_MAX]).is_ok());
    let key = server();
    let big = [0u8; MSG1_PAYLOAD_MAX + 1];
    assert!(matches!(
        Initiator::new(&key.public(), b"", &big),
        Err(HandshakeError::PayloadTooLarge)
    ));
    assert_eq!(
        Initiator::new(&key.public(), b"", &big[1..])
            .unwrap()
            .msg1()
            .len(),
        MSG1_LEN_MAX
    );
}

#[test]
fn a_forged_accept_does_not_end_the_handshake() {
    let key = server();
    let mut init = Initiator::new(&key.public(), b"ctx", b"").unwrap();
    let resp = Msg1::parse(init.msg1())
        .unwrap()
        .cookie_verified(&key, b"ctx", &accept(5))
        .unwrap();
    assert!(matches!(
        init.finish(&resp.msg2[1..]),
        Err(HandshakeError::Malformed)
    ));
    for at in [0, KEY_LEN, MSG2_LEN - 1] {
        let mut forged = resp.msg2.clone();
        forged[at] ^= 0x20;
        assert!(
            matches!(init.finish(&forged), Err(HandshakeError::Decrypt)),
            "byte {at}"
        );
    }
    // Another server session's genuine msg2 does not fit either.
    let other = Initiator::new(&key.public(), b"ctx", b"").unwrap();
    let stray = Msg1::parse(other.msg1())
        .unwrap()
        .cookie_verified(&key, b"ctx", &accept(5))
        .unwrap();
    assert!(matches!(
        init.finish(&stray.msg2),
        Err(HandshakeError::Decrypt)
    ));
    let Ok((got, _)) = init.finish(&resp.msg2) else {
        panic!("genuine msg2 after forgeries")
    };
    assert_eq!(got.cid, 5);
    assert!(
        matches!(init.finish(&resp.msg2), Err(HandshakeError::Internal)),
        "finished twice"
    );
}

#[test]
fn static_key_public_half_is_stable() {
    let a = server();
    let b = server();
    assert_ne!(a.public(), b.public());
    assert_ne!(a.public(), [0; KEY_LEN]);
}

#[test]
fn accept_encoding_round_trips_and_is_exact_length() {
    let a = accept(0x0102_0304_0506_0708);
    let b = a.encode();
    assert_eq!(b[..8], 0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(Accept::decode(&b), Some(a));
    assert_eq!(Accept::decode(&b[1..]), None);
    assert_eq!(Accept::decode(&[b.as_slice(), &[0]].concat()), None);
}

#[test]
fn reset_tokens_are_deterministic_per_key_and_cid() {
    let k = ResetKey::from_bytes([9; RESET_KEY_LEN]);
    let restarted = ResetKey::from_bytes([9; RESET_KEY_LEN]);
    assert_eq!(k.token(42), restarted.token(42), "survives a restart");
    assert_ne!(k.token(42), k.token(43));
    assert_ne!(
        k.token(42),
        ResetKey::from_bytes([8; RESET_KEY_LEN]).token(42)
    );
    let t = k.token(42).to_bytes();
    assert!(k.token(42).matches(&t));
    let mut flipped = t;
    flipped[RESET_TOKEN_LEN - 1] ^= 1;
    assert!(!k.token(42).matches(&flipped));
    assert!(!k.token(42).matches(&t[1..]));
    assert_eq!(format!("{:?}", k.token(42)), "ResetToken(..)");
}
