//! The writer's record layer (B5a): on a sealed session every datagram
//! it sends — a control frame, its re-send, a probe, the demux's ACK and
//! path challenge — is a SEALED record the client opens, each under a
//! fresh counter; the budget it fragments to is the inner datagram's; and
//! an exhausted counter ends the session, counted.

use super::*;
use crate::seal::wire::OVERHEAD_S2C;
use crate::seal::{Accept, Initiator, Msg1, Opener, ResetToken, SEAL_LIMIT, StaticKey};
use crate::udp::sealed::encode_send;

/// A sealed writer (budget 1200) toward a client socket, the client's
/// opener, and the actor's inbox.
async fn sealed_writer() -> (
    UdpWriter,
    UdpSocket,
    Opener,
    gsb_core::channel::Inbox<ConnIn>,
) {
    let server = StaticKey::generate().unwrap();
    let mut ini = Initiator::new(&server.public(), b"ctx", &[]).unwrap();
    let accept = Accept {
        cid: 7,
        reset_token: ResetToken::from_bytes([0; 16]),
    };
    let r = Msg1::parse(ini.msg1())
        .unwrap()
        .cookie_verified(&server, b"ctx", &accept)
        .unwrap();
    let (_, client) = ini.finish(&r.msg2).unwrap();
    let (sealer, _) = r.session.into_halves();
    let (_, opener) = client.into_halves();

    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let (reaper, _reap_rx) = Reaper::new(sock.clone());
    let link = spawn::Link {
        sock,
        peer: peer.local_addr().unwrap(),
        max_datagram: 1200,
        reaper,
        metrics: None,
        congestion: UdpCongestion::Off,
        sealer: Some(sealer),
    };
    let (in_tx, inbox) = channel::<ConnIn>(8);
    let (_out_tx, out_rx) = channel::<FrameBatch>(8);
    let w = UdpWriter::new(link, ConnectionId(3), in_tx, out_rx);
    (w, peer, opener, inbox)
}

/// The next record `s` receives, opened: `(counter, inner)`.
async fn opened(s: &UdpSocket, o: &mut Opener) -> (u64, Vec<u8>) {
    let mut buf = vec![0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), s.recv_from(&mut buf))
        .await
        .expect("a record")
        .expect("recv");
    let x = o.open(&buf[..n]).expect("the record opens");
    assert_eq!(n, x.plaintext.len() + OVERHEAD_S2C, "s→c overhead 25 B");
    (x.counter, x.plaintext)
}

#[tokio::test]
async fn every_datagram_of_a_sealed_writer_is_a_fresh_record() {
    let (mut w, client, mut o, _inbox) = sealed_writer().await;
    assert!(w.sealed());
    assert_eq!(w.max_datagram, 1200 - OVERHEAD_S2C, "the inner budget");

    let hb = FrameBody::new(op::base::HEARTBEAT_ACK, Bytes::from_static(b"hb"));
    assert!(w.send_batch(vec![hb.clone()]).await.is_none());
    let (c0, first) = opened(&client, &mut o).await;
    assert_eq!(first, encode_rel(1, &hb));

    // The re-send of the same frame: the same inner, a NEW counter.
    tokio::time::sleep(crate::udp::rel::INITIAL_RTO * 2).await;
    assert!(w.retransmit_pass().is_none());
    let (c1, again) = opened(&client, &mut o).await;
    assert_eq!((again, c1), (first, c0 + 1));

    // A probe.
    w.apply_report(&report(0, 0));
    w.probe_pass();
    let (c2, probe) = opened(&client, &mut o).await;
    assert_eq!((probe[0], c2), (KIND_PROBE, c1 + 1));

    // The demux's ACK (to the session) and a challenge (to a candidate).
    let other = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let send =
        |to, inner: &[u8]| FrameBody::new(op::base::UDP_SEND, Bytes::from(encode_send(to, inner)));
    let batch = vec![
        send(None, &encode_ack(5)),
        send(Some(other.local_addr().unwrap()), &encode_path_challenge(9)),
    ];
    assert!(w.send_batch(batch).await.is_none());
    assert_eq!(opened(&client, &mut o).await, (c2 + 1, encode_ack(5)));
    assert_eq!(
        opened(&other, &mut o).await,
        (c2 + 2, encode_path_challenge(9))
    );
    assert_eq!(w.unsent, 0, "transport requests are not session frames");
}

/// The record counter at its limit: nothing more is sealed, and the
/// session ends — the actor told (the stream rejected), counted.
#[tokio::test]
async fn an_exhausted_record_counter_ends_the_session() {
    let (mut w, _client, _o, mut inbox) = sealed_writer().await;
    w.sealer
        .as_mut()
        .unwrap()
        .set_next_counter_for_test(SEAL_LIMIT);
    let hb = FrameBody::new(op::base::HEARTBEAT_ACK, Bytes::new());
    assert!(w.send_batch(vec![hb]).await.is_none());
    assert!(w.seal_exhausted);
    w.die_sealed();
    assert_eq!(w.ended_seal_limit, 1);
    match inbox.try_recv() {
        Ok(ConnIn::ServerClosed { cause, .. }) => {
            assert_eq!(cause, gsb_core::conn::ServerClose::StreamRejected)
        }
        other => panic!("expected the close, got {other:?}"),
    }
}
