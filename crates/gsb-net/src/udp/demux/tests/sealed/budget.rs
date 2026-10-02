//! The global DH budget at the demux (B119), on a paused clock: checked
//! after the cookie and the per-source cap, BEFORE the Diffie-Hellman.

use super::*;

/// A one-token budget (10/s): the first proof takes it; the second is
/// refused — no session, no accept, counted — and its message 1 is never
/// processed: a proof whose message 1 would FAIL the DH (a wrong pinned
/// key) is counted as a budget refusal, not a decrypt failure. 100 ms
/// later the refused client's re-sent proof gets in.
#[tokio::test(start_paused = true)]
async fn the_budget_refuses_before_the_dh_and_refills_with_the_clock() {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, end_rx, server) = sealed_demux(sock, Some(10));
    let a = client();
    let b = client();
    let c = client();
    let _ = handshake(&mut d, &a, &server).await;

    // A wrong-key proof on the empty bucket: refused by the budget.
    let wrong = crate::seal::StaticKey::generate().unwrap().public();
    let cookie = cookie_for(&mut d, &c, 3).await;
    let (_, p) = proof(&wrong, 3, cookie);
    feed(&mut d, addr(&c), &p);
    let seal = d.seal.as_ref().unwrap();
    assert_eq!(
        (seal.budget.refused, seal.counts.handshakes_failed_decrypt),
        (1, 0),
        "no DH ran for a proof the budget refused"
    );

    let cookie = cookie_for(&mut d, &b, 4).await;
    let (mut ini, p) = proof(&server, 4, cookie);
    feed(&mut d, addr(&b), &p);
    assert_eq!(recv(&b).await, None, "refused: no accept");
    assert_eq!(d.seal.as_ref().unwrap().budget.refused, 2);
    assert_eq!((d.established, end_rx.len()), (1, 1));

    tokio::time::advance(Duration::from_millis(100)).await;
    feed(&mut d, addr(&b), &p);
    let acc = recv(&b).await.expect("the accept, a token later");
    assert!(ini.finish(&acc[5..]).is_ok());
    assert_eq!((d.established, end_rx.len()), (2, 2));
}

/// The per-source cap refuses BEFORE the budget: a refused proof takes no
/// token.
#[tokio::test(start_paused = true)]
async fn the_per_source_cap_spends_no_token() {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, _end_rx, server) = sealed_demux(sock, Some(10));
    d.per_source = crate::udp::demux::source::PerSource::new(Some(1));
    let a = client();
    let b = client();
    let _ = handshake(&mut d, &a, &server).await; // the token, and the place
    tokio::time::advance(Duration::from_millis(100)).await; // a new token
    let cookie = cookie_for(&mut d, &b, 8).await;
    let (_, p) = proof(&server, 8, cookie);
    feed(&mut d, addr(&b), &p); // the same source (127.0.0.1): capped
    assert_eq!(d.per_source.refused, 1);
    assert_eq!(d.seal.as_ref().unwrap().budget.refused, 0);
    assert_eq!(recv(&b).await, None);
    // The cap lifted, the same proof finds the token still there.
    d.per_source = crate::udp::demux::source::PerSource::new(None);
    feed(&mut d, addr(&b), &p);
    assert_eq!(recv(&b).await.map(|v| v.len()), Some(77));
    assert_eq!(d.seal.as_ref().unwrap().budget.refused, 0);
}
