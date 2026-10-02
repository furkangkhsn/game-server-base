//! Connection migration's handshake half, on a demux driven directly
//! (BACKLOG B3): who gets a CID, what the accept carries, and that an
//! older client — or a door with migration off — sees the accept it
//! always saw. The peer is a real socket, so every answer can be read.

use super::*;

/// A demux (migration `on` or off) with no sessions, plus a peer socket.
async fn door(on: bool) -> (Demux, crossbeam_channel::Receiver<Queued>, UdpSocket) {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, end_rx) = demux_bare(sock);
    d.migration = on;
    let peer = UdpSocket::bind("127.0.0.1:0").await.expect("peer");
    (d, end_rx, peer)
}

/// The next datagram `peer` receives (within 200 ms).
async fn answer(peer: &UdpSocket) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_millis(200), peer.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
        _ => None,
    }
}

/// `peer`'s proof, with the capability byte `caps` (0: none).
fn proof(d: &Demux, peer: SocketAddr, caps: u8) -> Vec<u8> {
    let nonce = 0x5EED_u64;
    encode_proof(nonce, d.cookie.compute(nonce, peer, d.clock.slot()), caps)
}

/// Migration on, the client asks: the accept is `ACK{1}` and an 8-byte
/// CID (13 B), the session is indexed by it, and a re-sent proof gets
/// the SAME CID back (idempotent: no second session, no second CID).
#[tokio::test]
async fn a_client_that_asks_gets_a_cid_in_the_accept_and_keeps_it() {
    let (mut d, end_rx, peer_sock) = door(true).await;
    let peer = peer_sock.local_addr().unwrap();
    let p = proof(&d, peer, CAP_CID);
    feed(&mut d, peer, &p);
    let accept = answer(&peer_sock).await.expect("the accept");
    assert_eq!(accept.len(), 13);
    assert_eq!(&accept[..5], &encode_ack(1)[..], "ACK{{1}} first");
    let cid = u64_at(&accept, 5).unwrap();
    assert_eq!(d.sessions[&peer].cid, Some(cid));
    assert_eq!(d.sessions.key_of_cid(cid), d.sessions.key_at(&peer));
    assert_eq!(d.mig.cids_assigned, 1);

    let p = proof(&d, peer, CAP_CID);
    feed(&mut d, peer, &p);
    assert_eq!(answer(&peer_sock).await, Some(accept), "the same CID again");
    assert_eq!((d.sessions.len(), end_rx.len()), (1, 1));
    assert_eq!(d.mig.cids_assigned, 1, "granted once");
}

/// CIDs are random, not a sequence: two sessions, two unrelated CIDs.
#[tokio::test]
async fn cids_are_drawn_not_counted() {
    let (mut d, _end_rx, a) = door(true).await;
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut cids = Vec::new();
    for s in [&a, &b] {
        let peer = s.local_addr().unwrap();
        let p = proof(&d, peer, CAP_CID);
        feed(&mut d, peer, &p);
        cids.push(u64_at(&answer(s).await.unwrap(), 5).unwrap());
    }
    assert_ne!(cids[0], cids[1]);
    assert!(cids[0].abs_diff(cids[1]) > 1_000, "{cids:x?}");
}

/// The compatibility matrix's server half. Migration off (the default),
/// a client asking or not: the accept is the 5-byte `ACK{1}` it always
/// was, and no CID exists. Migration on, a client that does not ask (an
/// older client): the same 5 bytes, and no CID.
#[tokio::test]
async fn without_both_sides_the_accept_is_the_old_five_bytes() {
    for (on, caps) in [(false, CAP_CID), (false, 0), (true, 0)] {
        let (mut d, end_rx, peer_sock) = door(on).await;
        let peer = peer_sock.local_addr().unwrap();
        let p = proof(&d, peer, caps);
        feed(&mut d, peer, &p);
        assert_eq!(
            answer(&peer_sock).await,
            Some(encode_ack(1)),
            "migration {on}, caps {caps}"
        );
        assert_eq!(d.sessions[&peer].cid, None);
        assert_eq!(d.mig.cids_assigned, 0);
        assert_eq!(end_rx.len(), 1, "established all the same");
    }
}

/// A door with migration off treats a tagged datagram as it always
/// treated an unknown kind: malformed, counted, the session untouched —
/// so a client that tagged by mistake costs nothing but a count.
#[tokio::test]
async fn with_migration_off_a_tagged_datagram_is_an_unknown_kind() {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let (mut d, mut in_rx) = demux_with_session(sock, peer);
    let seen = d.sessions[&peer].last_seen;
    let raw = encode_raw(&FrameBody::new(1000, Bytes::from_static(b"mv")));
    feed(&mut d, peer, &tag(7, &raw));
    assert_eq!(d.bad_datagrams, 1);
    assert!(in_rx.try_recv().is_err(), "nothing forwarded");
    assert_eq!(d.sessions[&peer].last_seen, seen, "not a sign of life");
    assert_eq!(d.mig.cid_unknown, 0, "not even read as a CID");
}
