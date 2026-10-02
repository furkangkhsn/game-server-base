//! The per-source cap on pending sessions (BACKLOG B89; module
//! `super::source`), on a demux driven directly: a verified proof over
//! the cap creates nothing, answers nothing and is counted; every end of
//! a pending session gives its place back; only proofs that passed the
//! cookie are ever counted.

use super::*;

const A: [u8; 4] = [127, 0, 0, 1];
const B: [u8; 4] = [127, 0, 0, 2];

/// A demux capping each source at `cap` pending sessions.
async fn door(cap: Option<usize>) -> (Demux, crossbeam_channel::Receiver<Queued>) {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, end_rx) = demux_bare(sock);
    d.per_source = source::PerSource::new(cap);
    (d, end_rx)
}

fn at(ip: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::from((ip, port))
}

/// `peer`'s valid proof (this slot's cookie).
fn proof(d: &Demux, peer: SocketAddr) -> Vec<u8> {
    let nonce = 0x0B89_u64 ^ u64::from(peer.port());
    encode_hello(nonce, d.cookie.compute(nonce, peer, d.clock.slot()))
}

/// What `s` receives within 150 ms.
async fn heard(s: &UdpSocket) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_millis(150), s.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
        _ => None,
    }
}

/// At its cap a source's next verified proof creates no session, gets
/// no accept and is counted; another source (127.0.0.2) is served; once
/// the accept loop takes the pending endpoint, the re-sent proof gets in.
#[tokio::test]
async fn a_source_at_its_cap_is_refused_until_a_pending_session_is_taken() {
    let (mut d, end_rx) = door(Some(1)).await;
    let first = at(A, 40001);
    let p = proof(&d, first);
    feed(&mut d, first, &p);
    assert_eq!((d.sessions.len(), end_rx.len()), (1, 1));

    let second = UdpSocket::bind("127.0.0.1:0").await.expect("second");
    let peer = second.local_addr().unwrap();
    let p2 = proof(&d, peer);
    feed(&mut d, peer, &p2);
    assert_eq!(heard(&second).await, None, "no accept over the cap");
    assert!(!d.sessions.contains_key(&peer), "no session");
    assert_eq!(end_rx.len(), 1, "no endpoint");
    assert_eq!(d.per_source.refused, 1);
    assert_eq!(d.established, 1);

    let other = at(B, 40001);
    let p = proof(&d, other);
    feed(&mut d, other, &p);
    assert!(d.sessions.contains_key(&other), "another source is served");
    assert_eq!(d.per_source.refused, 1);

    // The accept loop takes the first endpoint: its place comes back.
    drop(end_rx.try_recv().expect("queued").into_endpoint());
    feed(&mut d, peer, &p2);
    assert_eq!(
        heard(&second).await,
        Some(encode_ack(1)),
        "the re-send gets in"
    );
    assert_eq!(d.per_source.pending_from(peer.ip()), 1);
    assert_eq!(d.per_source.refused, 1);
}

/// Only a proof that passed the cookie counts: challenge requests and
/// forged or expired proofs from a source (what an off-path spoofer can
/// send with a victim's address) take no place, so the source's real
/// proof is still served at a cap of one.
#[tokio::test]
async fn a_spoofed_source_takes_no_place() {
    let (mut d, end_rx) = door(Some(1)).await;
    let victim = at(A, 40002);
    for i in 0..3u64 {
        feed(&mut d, victim, &encode_hello(i, 0));
        feed(&mut d, victim, &encode_hello(i, 0xBAD ^ i));
    }
    assert_eq!(d.bad_cookie, 3);
    assert_eq!(
        d.per_source.sources(),
        0,
        "nothing counted before the cookie"
    );
    let p = proof(&d, victim);
    feed(&mut d, victim, &p);
    assert!(d.sessions.contains_key(&victim), "the real proof is served");
    assert_eq!((end_rx.len(), d.per_source.refused), (1, 0));
}

/// Every end of a pending session gives its place back: taken by the
/// accept loop, dropped with the listener's queue, torn down at a full
/// endpoint channel. The table empties with the last claim.
#[tokio::test]
async fn every_end_of_a_pending_session_gives_its_place_back() {
    let (mut d, end_rx) = door(Some(8)).await;
    // The harness's endpoint channel holds four: the fifth is torn down.
    for port in 1..=5 {
        let peer = at(A, 41000 + port);
        let p = proof(&d, peer);
        feed(&mut d, peer, &p);
    }
    assert_eq!(d.endpoints_dropped, 1);
    assert_eq!(
        d.per_source.pending_from(A.into()),
        4,
        "the torn-down one is back"
    );
    drop(end_rx.try_recv().unwrap().into_endpoint());
    assert_eq!(d.per_source.pending_from(A.into()), 3, "taken");
    drop(end_rx.try_recv().unwrap());
    assert_eq!(d.per_source.pending_from(A.into()), 2, "dropped unaccepted");
    while end_rx.try_recv().is_ok() {}
    assert_eq!(d.per_source.sources(), 0, "the table empties");
    assert_eq!(d.per_source.refused, 0);
}

/// No cap (the default): one source gets every session it proves and
/// the table holds nothing.
#[tokio::test]
async fn with_no_cap_nothing_is_counted() {
    for cap in [None, Some(0)] {
        let (mut d, end_rx) = door(cap).await;
        for port in 1..=4 {
            let peer = at(A, 42000 + port);
            let p = proof(&d, peer);
            feed(&mut d, peer, &p);
        }
        assert_eq!(end_rx.len(), 4, "{cap:?}");
        assert_eq!(d.per_source.sources(), 0, "{cap:?}");
        assert_eq!(d.per_source.refused, 0, "{cap:?}");
    }
}
