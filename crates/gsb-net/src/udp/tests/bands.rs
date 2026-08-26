//! The two bands under stress: the datagram budget (drop, never
//! fragment), the reliable band's retransmit clock, and idle teardown.
//! Child of `tests`, so `bound_transport` is shared, not duplicated.

use super::*;

/// MTU policy: an outbound datagram over the budget is dropped and
/// counted (never fragmented, v1 constraint) while smaller frames on
/// the same session — control AND game band — are still delivered.
#[tokio::test]
async fn oversized_outbound_is_dropped_not_fragmented() {
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
    let (_r, _w) = ep.start_pump(ConnectionId(1), in_tx, out_rx, None);

    // RAW band, 25-byte payload → 28-byte datagram: fits.
    out_tx
        .send(vec![FrameBody::new(
            1000,
            Bytes::copy_from_slice(&[1u8; 25]),
        )])
        .await
        .unwrap();
    let ok = tokio::time::timeout(Duration::from_secs(3), client.recv_frame(Duration::from_millis(200)))
        .await
        .expect("window")
        .expect("recv")
        .expect("the in-budget frame must arrive");
    assert_eq!(ok.payload.len(), 25);

    // RAW band, 40-byte payload → 43-byte datagram: over the 40-byte
    // budget. Dropped, not fragmented.
    out_tx
        .send(vec![FrameBody::new(
            1000,
            Bytes::copy_from_slice(&[2u8; 40]),
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
    let ctl = tokio::time::timeout(Duration::from_secs(3), client.recv_frame(Duration::from_millis(1000)))
        .await
        .expect("window")
        .expect("recv")
        .expect("control must still be delivered");
    assert_eq!(ctl.op, gsb_protocol::op::base::HEARTBEAT_ACK);

    // The oversized frame must NOT arrive (neither whole nor
    // fragmented): the next frame is the control one.
    let next = tokio::time::timeout(
        Duration::from_millis(600),
        client.recv_frame(Duration::from_millis(400)),
    )
    .await
    .expect("window")
    .expect("recv");
    assert!(next.is_none(), "the oversized frame must be dropped, not delivered");
}

/// Reliability, server side: a control frame the client never ACKs is
/// retransmitted on the RTO (the raw client observes the same seq
/// again with an identical payload).
#[tokio::test]
async fn server_retransmits_until_ack() {
    let (_listener, addr, mut eps, _accept) =
        bound_transport(UdpTransportConfig::default()).await;

    // Raw client (full ACK control).
    let raw = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind");
    let mut buf = vec![0u8; 2048];
    let nonce = 0x0123_4567_89AB_CDEFu64;
    raw.send_to(&encode_hello(nonce, 0), addr)
        .await
        .unwrap();
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
    let (_r, _w) = ep.start_pump(ConnectionId(1), in_tx, out_rx, None);
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
    assert_eq!(first, second, "the retransmit is byte-identical (same seq, same payload)");
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
