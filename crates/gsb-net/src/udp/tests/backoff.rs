//! The server writer's retransmit timer backs off (BACKLOG B2): a
//! control frame the peer never ACKs is re-sent at 50, 150, 350, 750 ms…
//! after its first send (the timer doubling from each re-send), not
//! every 50 ms. Counted, not timed: a timer can only fire late, so the
//! copies inside a window are at most the schedule's — a loaded machine
//! makes them fewer, never more.

use super::*;

#[tokio::test]
async fn an_unanswered_control_frame_is_re_sent_on_a_doubling_schedule() {
    let (_listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let raw = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    raw_handshake(&raw, addr, 0xB2B2_0000_B2B2_0001).await;
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

    // The window opens BEFORE the frame is handed over, so every copy's
    // send time is inside it: the first send, then re-sends at 50, 150,
    // 350 ms (the next is due at 750 ms, past the window).
    let window = Duration::from_millis(700);
    let t0 = tokio::time::Instant::now();
    out_tx
        .send(vec![FrameBody::new(
            gsb_protocol::op::base::ERROR,
            Bytes::from(vec![9, 0]),
        )])
        .await
        .unwrap();
    let mut buf = vec![0u8; 2048];
    let mut copies = 0;
    while let Ok(got) = tokio::time::timeout_at(t0 + window, raw.recv_from(&mut buf)).await {
        let (n, _) = got.expect("recv");
        assert_eq!(buf[0], KIND_REL, "{:?}", &buf[..n]);
        assert_eq!(u32::from_le_bytes(buf[1..5].try_into().unwrap()), 1);
        copies += 1;
    }
    // A fixed 50 ms timer would have sent ~14.
    assert!(
        (2..=4).contains(&copies),
        "{copies} copies in {window:?}: the first and at most three re-sends"
    );
}
