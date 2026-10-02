//! The client's half of connection migration (module `crate::udp::path`):
//! the CID comes only from an accept it asked for, every datagram after
//! it is tagged (and nothing before it), a path challenge is echoed at
//! once — and [`UdpClient::rebind`] moves the socket, or refuses without
//! a CID.

use super::*;

const CID: u64 = 0xC1D0_C1D0_C1D0_C1D0;

/// The next datagram the peer received (bounded wait), or `None`.
async fn next(sink: &UdpSocket) -> Option<(Vec<u8>, SocketAddr)> {
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_millis(200), sink.recv_from(&mut buf)).await {
        Ok(Ok((n, from))) => Some((buf[..n].to_vec(), from)),
        _ => None,
    }
}

fn accept_with_cid() -> Vec<u8> {
    encode_accept(1, Some(CID))
}

/// The CID is taken from the accept only when this client asked, and
/// only when the accept carries one.
#[tokio::test]
async fn the_cid_comes_from_an_accept_the_client_asked_for() {
    let (mut c, _sink) = detached().await;
    c.take_cid(&encode_ack(1));
    assert!(!c.migratable(), "a 5-byte accept grants nothing");
    c.take_cid(&accept_with_cid());
    assert_eq!(c.cid(), Some(CID));

    let off = UdpClientConfig {
        migration: false,
        ..Default::default()
    };
    let (mut old, _sink) = detached_with(off).await;
    old.take_cid(&accept_with_cid());
    assert!(!old.migratable(), "a client that did not ask takes nothing");
}

/// With a CID every datagram is tagged — a control frame (and so its
/// re-send), a game frame, a report, an ACK; without one, none is.
#[tokio::test]
async fn every_datagram_after_the_grant_is_tagged() {
    let (mut c, sink) = detached().await;
    c.send_frame(1000, Bytes::from_static(b"mv")).await.unwrap();
    let (untagged, _) = next(&sink).await.unwrap();
    assert_eq!(untagged[0], KIND_RAW, "no CID: the old bytes");

    c.take_cid(&accept_with_cid());
    c.send_frame(1000, Bytes::from_static(b"mv")).await.unwrap();
    assert_eq!(next(&sink).await.unwrap().0, tag(CID, &untagged));
    c.send_frame(gsb_protocol::op::base::HEARTBEAT, Bytes::new())
        .await
        .unwrap();
    let rel = next(&sink).await.unwrap().0;
    assert_eq!(
        (rel[0], u64_at(&rel, 1)),
        (KIND_REL | KIND_CID_TAG, Some(CID))
    );
    c.rel.front_mut().unwrap().sent -= Duration::from_secs(1);
    c.retransmit_pass();
    let resent = next(&sink).await.unwrap().0;
    assert_eq!(resent, rel, "the re-send is the tagged datagram");
    let announce = next(&sink).await.unwrap().0;
    assert_eq!(announce, tag(CID, &encode_report(0, 0)), "the report too");
    c.process_datagram(&rel_s2c());
    let ack = next(&sink).await.unwrap().0;
    assert_eq!(ack, tag(CID, &encode_ack(2)));
}

fn rel_s2c() -> Vec<u8> {
    encode_rel(
        1,
        &FrameBody::new(gsb_protocol::op::base::ERROR, Bytes::from_static(&[1])),
    )
}

/// A challenge is echoed at once, tagged; without a CID it is ignored
/// (counted) — a server challenges only a session that has one.
#[tokio::test]
async fn a_challenge_is_echoed_with_the_cid() {
    let (mut c, sink) = detached().await;
    c.process_datagram(&encode_path_challenge(42));
    assert_eq!(next(&sink).await, None);
    assert_eq!(c.stats.path_challenges_ignored, 1);
    c.take_cid(&accept_with_cid());
    c.process_datagram(&encode_path_challenge(42));
    let (resp, _) = next(&sink).await.unwrap();
    assert_eq!(resp, encode_path_response(CID, 42));
    assert_eq!(c.stats.path_challenges_answered, 1);
}

/// `rebind` moves the client to a new local socket and announces it at
/// once with a tagged ACK from there; without a CID it refuses and the
/// socket stays.
#[tokio::test]
async fn rebind_moves_the_socket_and_needs_a_cid() {
    let (mut c, sink) = detached().await;
    let before = c.local_addr().unwrap();
    let e = c.rebind().await.expect_err("no CID");
    assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(c.local_addr(), Some(before), "nothing changed");

    c.take_cid(&accept_with_cid());
    let after = c.rebind().await.expect("rebind");
    assert_ne!(after.port(), before.port());
    let (nudge, from) = next(&sink).await.expect("the nudge");
    assert_eq!(from, after, "from the new socket");
    assert_eq!(nudge, tag(CID, &encode_ack(1)));
    assert_eq!(c.stats.rebinds, 1);
}
