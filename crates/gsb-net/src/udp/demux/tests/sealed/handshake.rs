//! The sealed handshake's refusals on the demux: a re-sent proof gets the
//! stored accept (no second DH); plaintext, malformed and wrong-key
//! proofs create nothing.

use super::*;

/// Idempotent proof: the same proof again, before the session's first
/// record, gets the SAME accept bytes — the stored message 2, no second
/// Diffie-Hellman (the one-token budget would have refused one, and a
/// recomputed message 2 differs: a fresh ephemeral key). Once a record
/// opened, the stale proof gets nothing.
#[tokio::test(start_paused = true)]
async fn a_resent_proof_gets_the_stored_accept_without_a_second_dh() {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, end_rx, server) = sealed_demux(sock, Some(10));
    let a = client();
    let (_, session, acc, p) = handshake(&mut d, &a, &server).await;
    feed(&mut d, addr(&a), &p);
    assert_eq!(recv(&a).await, Some(acc.clone()), "the same bytes");
    feed(&mut d, addr(&a), &p);
    assert_eq!(recv(&a).await, Some(acc), "and again");
    let seal = d.seal.as_ref().unwrap();
    assert_eq!(seal.budget.refused, 0, "no DH was even asked for");
    assert_eq!((d.proofs_reanswered, d.established), (2, 1));
    assert_eq!(end_rx.len(), 1, "one session");

    let (mut tx, _) = session.into_halves();
    let mut rec = Vec::new();
    tx.seal(&encode_ack(1), &mut rec).unwrap();
    feed(&mut d, addr(&a), &rec);
    feed(&mut d, addr(&a), &p);
    assert_eq!(
        recv(&a).await,
        None,
        "confirmed: the stale proof is ignored"
    );
    assert_eq!(d.proofs_reanswered, 2);
}

/// A plaintext client at a sealed door: its proof (18 or 19 bytes, no
/// message 1) verifies but is refused, counted — no session, no accept.
/// A proof whose message 1 has a wrong length is refused before any DH
/// (the one token is still there for the next good proof).
#[tokio::test(start_paused = true)]
async fn plaintext_and_malformed_proofs_are_refused_before_any_dh() {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, end_rx, server) = sealed_demux(sock, Some(10));
    let a = client();
    let cookie = cookie_for(&mut d, &a, 5).await;
    feed(&mut d, addr(&a), &encode_hello(5, cookie));
    feed(&mut d, addr(&a), &encode_proof(5, cookie, CAP_CID));
    let (_, p) = proof(&server, 5, cookie);
    feed(&mut d, addr(&a), &p[..p.len() - 1]);
    feed(&mut d, addr(&a), &[&p[..], &[0u8; 40]].concat());
    assert_eq!(recv(&a).await, None, "nothing answers a refused proof");
    let c = d.seal.as_ref().unwrap().counts;
    assert_eq!((c.proofs_refused_plaintext, c.handshakes_malformed), (2, 2));
    assert!(end_rx.is_empty() && d.sessions.is_empty());
    feed(&mut d, addr(&a), &p);
    assert_eq!(
        recv(&a).await.map(|v| v.len()),
        Some(77),
        "the token was kept"
    );
}

/// A client pinning another server key: message 1 does not authenticate
/// after the DH — counted as a decrypt failure, nothing created.
#[tokio::test]
async fn a_client_pinning_another_key_fails_the_handshake() {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, end_rx, _server) = sealed_demux(sock, None);
    let a = client();
    let other = StaticKey::generate().unwrap().public();
    let cookie = cookie_for(&mut d, &a, 6).await;
    let (_, p) = proof(&other, 6, cookie);
    feed(&mut d, addr(&a), &p);
    assert_eq!(recv(&a).await, None);
    assert_eq!(d.seal.as_ref().unwrap().counts.handshakes_failed_decrypt, 1);
    assert!(end_rx.is_empty() && d.sessions.is_empty());
}
