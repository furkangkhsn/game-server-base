//! The server half of the handshake's loss healing, on a demux driven
//! directly: a proof re-sent by a peer that already has a session is
//! answered (the accept again) and never becomes a second session, and a
//! re-send across a cookie rotation still verifies. The peer is a real
//! bound socket, so every answer the demux sends can be read back.

use super::*;

/// A demux with no sessions, plus a real peer socket and its address.
async fn demux_and_peer() -> (Demux, crossbeam_channel::Receiver<Endpoint>, UdpSocket) {
    let sock = Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    );
    // The demux answers with `try_send_to`, which needs the reactor to
    // have seen the socket writable once (the live demux loop has).
    sock.writable().await.expect("writable");
    let (d, end_rx) = demux_bare(sock);
    let peer = UdpSocket::bind("127.0.0.1:0").await.expect("peer");
    (d, end_rx, peer)
}

/// The next datagram the peer receives, if one comes within 200 ms.
async fn answer(peer: &UdpSocket) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_millis(200), peer.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
        _ => None,
    }
}

/// A duplicate proof is idempotent: one session, one endpoint (so the
/// accept loop mints one `ConnectionId`), and each copy is answered with
/// the session's CURRENT cumulative ACK — after the peer's AUTH landed
/// that is 2, not a fresh session's 1.
#[tokio::test]
async fn a_duplicate_proof_is_answered_and_never_a_second_session() {
    let (mut d, end_rx, peer_sock) = demux_and_peer().await;
    let peer = peer_sock.local_addr().unwrap();
    let nonce = 0xD0D0_u64;
    let proof = encode_hello(nonce, d.cookie.compute(nonce, peer, d.clock.slot()));

    feed(&mut d, peer, &proof);
    assert_eq!(answer(&peer_sock).await, Some(encode_ack(1)), "the accept");
    assert_eq!(end_rx.len(), 1);

    feed(&mut d, peer, &proof);
    assert_eq!(
        answer(&peer_sock).await,
        Some(encode_ack(1)),
        "the accept again"
    );
    feed(
        &mut d,
        peer,
        &rel_frame(1, gsb_protocol::op::base::AUTH_REQ, b"a"),
    );
    assert_eq!(answer(&peer_sock).await, Some(encode_ack(2)), "AUTH's ACK");
    feed(&mut d, peer, &proof);
    assert_eq!(
        answer(&peer_sock).await,
        Some(encode_ack(2)),
        "a late copy reports the session as it is"
    );

    assert_eq!(d.sessions.len(), 1);
    assert_eq!(d.established, 1, "established once");
    assert_eq!(d.proofs_reanswered, 2);
    assert_eq!(end_rx.len(), 1, "one endpoint, one ConnectionId");
    assert_eq!(
        d.sessions[&peer].in_expected, 2,
        "the session's reliable state was not reset by a re-send"
    );

    // A known peer's challenge request, or a proof that does not verify,
    // is still answered with nothing (no reflection off a session).
    feed(&mut d, peer, &encode_hello(nonce, 0));
    feed(&mut d, peer, &encode_hello(nonce, 0xBAD));
    assert_eq!(answer(&peer_sock).await, None);
    assert_eq!(d.proofs_reanswered, 2);
}

/// The rotation argument, both halves. The client's first proof is lost
/// and its re-send arrives one slot later: it still establishes (the
/// previous-slot grace). A copy arriving two slots after its minting is
/// expired: dropped, counted, unanswered — and the session it would
/// have duplicated is untouched.
#[tokio::test]
async fn a_proof_re_sent_across_a_rotation_establishes_and_an_expired_one_is_dropped() {
    let (mut d, end_rx, peer_sock) = demux_and_peer().await;
    let peer = peer_sock.local_addr().unwrap();
    d.clock = CookieClock::started_at(Instant::now() - COOKIE_SLOT * 5);
    let nonce = 0xA0A0_u64;

    // The challenge, minted in slot 5 and read off the wire.
    feed(&mut d, peer, &encode_hello(nonce, 0));
    let challenge = answer(&peer_sock).await.expect("the challenge");
    let cookie = u64::from_le_bytes(challenge[9..17].try_into().unwrap());
    // The first proof is lost; the rotation happens (slot 6)...
    d.clock = CookieClock::started_at(Instant::now() - COOKIE_SLOT * 6);
    feed(&mut d, peer, &encode_hello(nonce, cookie));
    assert_eq!(answer(&peer_sock).await, Some(encode_ack(1)), "established");
    assert_eq!(end_rx.len(), 1);

    // ...and one more (slot 7): the same proof is now two slots old.
    d.clock = CookieClock::started_at(Instant::now() - COOKIE_SLOT * 7);
    feed(&mut d, peer, &encode_hello(nonce, cookie));
    assert_eq!(
        answer(&peer_sock).await,
        None,
        "an expired copy is unanswered"
    );
    assert_eq!(d.proofs_reanswered, 0);
    assert_eq!(d.sessions.len(), 1, "and the session is untouched");
    assert_eq!(end_rx.len(), 1);
}
