//! The two bands under stress: the datagram budget (fragment the game
//! band, drop past the ceiling), the reliable band's retransmit clock,
//! and idle teardown. Child of `tests`, so `bound_transport` is shared,
//! not duplicated.

use super::*;

/// MTU policy: a game-band frame over the budget is FRAGMENTED and
/// arrives whole; one past the fragment ceiling is dropped and counted
/// (nothing of it reaches the wire); smaller frames on the same session
/// — control AND game band — are delivered exactly as before.
#[tokio::test]
async fn oversized_game_frame_is_fragmented_and_past_the_ceiling_dropped() {
    let cfg = UdpTransportConfig {
        max_datagram_bytes: 40,
        ..Default::default()
    };
    let (_listener, addr, mut eps, _accept) = bound_transport(cfg).await;

    let mut client = UdpClient::connect(addr).await.expect("handshake");
    let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("endpoint")
        .expect("endpoint");
    let (in_tx, _in_rx) = ep.take_inbox(16);
    let (out_tx, out_rx) = ep.take_outbox(16);
    let (_r, _w) = ep.start_pump(
        ConnectionId(1),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    let recv = async |client: &mut UdpClient, wait: u64| {
        tokio::time::timeout(
            Duration::from_secs(3),
            client.recv_frame(Duration::from_millis(wait)),
        )
        .await
        .expect("window")
        .expect("recv")
    };

    // RAW band, 25-byte payload → 28-byte datagram: fits.
    out_tx
        .send(vec![FrameBody::new(
            1000,
            Bytes::copy_from_slice(&[1u8; 25]),
        )])
        .await
        .unwrap();
    let ok = recv(&mut client, 200)
        .await
        .expect("the in-budget frame must arrive");
    assert_eq!(ok.payload.as_ref(), &[1u8; 25]);
    assert_eq!(client.stats.frag_reassembled, 0, "it was not fragmented");

    // RAW band, 40-byte payload → 43-byte datagram: over the 40-byte
    // budget, so it travels as two FRAG datagrams and arrives whole.
    out_tx
        .send(vec![FrameBody::new(
            1000,
            Bytes::copy_from_slice(&[2u8; 40]),
        )])
        .await
        .unwrap();
    let whole = recv(&mut client, 1000)
        .await
        .expect("the over-budget frame must arrive whole");
    assert_eq!(whole.op, 1000);
    assert_eq!(whole.payload.as_ref(), &[2u8; 40]);
    assert_eq!(client.stats.frag_reassembled, 1);

    // Past the ceiling: 16 chunks of 35 bytes carry 560 bytes, this one
    // needs 602. Dropped and counted server-side; nothing is sent.
    out_tx
        .send(vec![FrameBody::new(
            1000,
            Bytes::copy_from_slice(&[3u8; 600]),
        )])
        .await
        .unwrap();

    // A control frame (small) is still delivered on the same session.
    out_tx
        .send(vec![FrameBody::new(
            gsb_protocol::op::base::HEARTBEAT_ACK,
            Bytes::from(vec![1, 2]),
        )])
        .await
        .unwrap();
    let ctl = recv(&mut client, 1000)
        .await
        .expect("control must still be delivered");
    assert_eq!(ctl.op, gsb_protocol::op::base::HEARTBEAT_ACK);

    // The past-ceiling frame never arrives, not even in part: no
    // fragment of it reached the client.
    let next = recv(&mut client, 400).await;
    assert!(next.is_none(), "the past-ceiling frame must be dropped");
    assert_eq!(client.stats.frag_reassembled, 1);
    assert_eq!(client.stats.frag_dropped_incomplete, 0);
    assert_eq!(client.stats.frag_rejected, 0, "no fragment of it was sent");
}

/// Reliability, server side: a control frame the client never ACKs is
/// retransmitted on the RTO (the raw client observes the same seq
/// again with an identical payload).
#[tokio::test]
async fn server_retransmits_until_ack() {
    let (_listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;

    // Raw client (full ACK control).
    let raw = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind");
    let mut buf = vec![0u8; 2048];
    let nonce = 0x0123_4567_89AB_CDEFu64;
    raw.send_to(&encode_hello(nonce, 0), addr).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
        .await
        .expect("challenge")
        .expect("recv");
    let cookie = u64::from_le_bytes(buf[9..17].try_into().unwrap());
    raw.send_to(&encode_hello(nonce, cookie), addr)
        .await
        .unwrap();

    // Fake actor: one control frame on the outbound channel.
    let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("endpoint")
        .expect("endpoint");
    let (in_tx, _in_rx) = ep.take_inbox(16);
    let (out_tx, out_rx) = ep.take_outbox(16);
    let (_r, _w) = ep.start_pump(
        ConnectionId(1),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    out_tx
        .send(vec![FrameBody::new(
            gsb_protocol::op::base::ERROR,
            Bytes::from(vec![9, 0]),
        )])
        .await
        .unwrap();

    // First delivery: REL seq 1.
    let (n1, _) = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
        .await
        .expect("first delivery")
        .expect("recv");
    let first = buf[..n1].to_vec();
    assert_eq!(first[0], KIND_REL);
    let seq1 = u32::from_le_bytes(first[1..5].try_into().unwrap());
    assert_eq!(seq1, 1);

    // No ACK. The RTO (50 ms) must retransmit the SAME frame.
    let (n2, _) = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
        .await
        .expect("retransmit must arrive")
        .expect("recv");
    let second = buf[..n2].to_vec();
    assert_eq!(
        first, second,
        "the retransmit is byte-identical (same seq, same payload)"
    );
}

/// The REL liveness bound, memory arm: against a peer that completes
/// the handshake and then ACKs NOTHING, the un-ACKed queue fills to
/// [`RETRANSIT_CAP`] and the writer ends the session — loudly, through
/// the connection ACTOR's mailbox (an in-process channel), not by
/// quietly dropping a frame and continuing. This is the arm that fires
/// in milliseconds; the wall-clock arm ([`REL_NO_ACK_FATAL`]) and the
/// room-slot release it causes are locked end to end in
/// `gsb-server/tests/udp_rel_liveness.rs`.
#[tokio::test]
async fn unacked_control_band_closes_the_session_at_the_memory_bound() {
    let (_listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;

    // A raw client: it handshakes and then never sends another byte —
    // in particular it never ACKs a single REL frame.
    let raw = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind");
    let mut buf = vec![0u8; 2048];
    let nonce = 0x5EED_1234_5EED_1234u64;
    raw.send_to(&encode_hello(nonce, 0), addr).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
        .await
        .expect("challenge")
        .expect("recv");
    let cookie = u64::from_le_bytes(buf[9..17].try_into().unwrap());
    raw.send_to(&encode_hello(nonce, cookie), addr)
        .await
        .unwrap();

    let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("endpoint")
        .expect("endpoint");
    let (in_tx, mut in_rx) = ep.take_inbox(16);
    let (out_tx, out_rx) = ep.take_outbox(16);
    let (_r, _w) = ep.start_pump(
        ConnectionId(1),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );

    // Feed control-band frames until the bound is crossed. Every one of
    // them is reliable, so every one of them stays outstanding.
    let feeder = tokio::spawn(async move {
        for i in 0..(RETRANSIT_CAP + 8) {
            let fb = FrameBody::new(
                gsb_protocol::op::base::HEARTBEAT_ACK,
                Bytes::from(vec![(i & 0xFF) as u8]),
            );
            if out_tx.send(vec![fb]).await.is_err() {
                break; // the writer is gone: the bound fired
            }
        }
    });

    let closed = tokio::time::timeout(Duration::from_secs(5), in_rx.recv())
        .await
        .expect("the bound must close the session well inside 5 s")
        .expect("the actor must be told, not left waiting");
    match closed {
        gsb_core::conn::ConnIn::ServerClosed { cause, reason } => {
            assert_eq!(cause, gsb_core::conn::ServerClose::RelDead);
            assert!(
                reason.contains("reliable control band"),
                "the close must name the reliable band: {reason}"
            );
        }
        other => panic!("expected ServerClosed, got {other:?}"),
    }
    feeder.abort();
    drop(raw);
}

/// Idle teardown (item: no FIN in UDP — the previous turn's
/// `idle_timeout` must work): a silent client's session is swept and
/// its actor gets `ConnIn::ServerClosed` on its inbound channel.
#[tokio::test]
async fn idle_sweep_delivers_server_closed() {
    let cfg = UdpTransportConfig {
        idle_timeout: Some(Duration::from_millis(200)),
        ..Default::default()
    };
    let (_listener, addr, mut eps, _accept) = bound_transport(cfg).await;

    let client = UdpClient::connect(addr).await.expect("handshake");
    let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("endpoint")
        .expect("endpoint");
    let (_in_tx, mut in_rx) = ep.take_inbox(16);
    // The client stays connected but SILENT: the handshake datagrams
    // are its last inbound traffic. The demux deadline (200 ms) must
    // fire and remove the session.
    let closed = tokio::time::timeout(Duration::from_secs(3), in_rx.recv())
        .await
        .expect("the sweep must deliver within 3 s")
        .expect("the sweep must deliver an item");
    match closed {
        gsb_core::conn::ConnIn::ServerClosed { .. } => {}
        other => panic!("expected ServerClosed, got {other:?}"),
    }
    drop(client);
}
