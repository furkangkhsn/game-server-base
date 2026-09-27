//! Fragmentation over a real socket: the server writer splits, the
//! client reassembles; the bytes of every datagram that is NOT an
//! over-budget game frame are pinned to what they were before FRAG
//! existed; and the control band never fragments. Child of `tests`, so
//! `bound_transport` is shared.

use super::*;
use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::{ConnIn, ServerClose};

/// A raw socket that completes the handshake by hand (so every datagram
/// the writer sends can be read byte for byte), plus its session's pump.
async fn raw_session(
    cfg: UdpTransportConfig,
) -> (UdpSocket, SocketAddr, Mailbox<FrameBatch>, Inbox<ConnIn>) {
    // The accept task holds its own listener handle: the demux lives on.
    let (_listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let raw = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind");
    raw_handshake(&raw, addr, 0xF4A6_0000_0000_0001u64).await;
    let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("endpoint")
        .expect("endpoint");
    let (in_tx, in_rx) = ep.take_inbox(16);
    let (out_tx, out_rx) = ep.take_outbox(16);
    let _ = ep.start_pump(
        ConnectionId(1),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    (raw, addr, out_tx, in_rx)
}

/// The next datagram the raw socket receives, if one comes within `ms`.
async fn next_datagram(raw: &UdpSocket, ms: u64) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 65536];
    match tokio::time::timeout(Duration::from_millis(ms), raw.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
        _ => None,
    }
}

/// An arena/MMO-sized game frame (10 000 bytes: arena 1000's peak full
/// was 10 267) goes out as fragments and comes back out of the client's
/// `recv_frame` byte-identical; a small frame after it is unaffected, and
/// the next large frame (the next message id) arrives too.
#[tokio::test]
async fn a_large_game_frame_round_trips_through_writer_and_client() {
    let (_listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let mut client = UdpClient::connect(addr).await.expect("handshake");
    let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("endpoint")
        .expect("endpoint");
    let (in_tx, _in_rx) = ep.take_inbox(16);
    let (out_tx, out_rx) = ep.take_outbox(16);
    let _pump = ep.start_pump(
        ConnectionId(1),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );

    let payload: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let second: Vec<u8> = payload.iter().rev().copied().collect();
    out_tx
        .send(vec![
            FrameBody::new(1201, Bytes::from(payload.clone())),
            FrameBody::new(1202, Bytes::from_static(&[9, 9])),
            FrameBody::new(1201, Bytes::from(second.clone())),
        ])
        .await
        .unwrap();
    let mut got = Vec::new();
    while got.len() < 3 {
        let f = tokio::time::timeout(
            Duration::from_secs(3),
            client.recv_frame(Duration::from_millis(1000)),
        )
        .await
        .expect("window")
        .expect("recv")
        .expect("all three frames must arrive");
        got.push(f);
    }
    assert_eq!(got[0].op, 1201);
    assert_eq!(got[0].payload.as_ref(), &payload[..], "byte-identical");
    assert_eq!(got[1].op, 1202, "the small frame follows, unfragmented");
    assert_eq!(got[1].payload.as_ref(), &[9, 9]);
    assert_eq!(got[2].payload.as_ref(), &second[..], "the next message");
    assert_eq!(client.stats.frag_reassembled, 2);
    assert_eq!(client.stats.frag_dropped_incomplete, 0);
}

/// Pin: the datagrams that are NOT over-budget game frames keep their
/// exact pre-FRAG bytes — RAW `[0][op][payload]`, REL `[1][seq][op]
/// [payload]` — up to a RAW frame exactly AT the budget; and the first
/// byte past it becomes FRAG kind 4 with the RAW body cut in order.
#[tokio::test]
async fn datagrams_within_the_budget_are_byte_identical() {
    let (raw, addr, out_tx, _in_rx) = raw_session(UdpTransportConfig::default()).await;

    let at_budget = vec![0x5Au8; DEFAULT_MAX_DATAGRAM_BYTES - 3];
    out_tx
        .send(vec![
            FrameBody::new(1003, Bytes::from_static(&[1, 2, 3])),
            FrameBody::new(gsb_protocol::op::base::ERROR, Bytes::from_static(&[8, 7])),
            FrameBody::new(1101, Bytes::from(at_budget.clone())),
        ])
        .await
        .unwrap();
    assert_eq!(
        next_datagram(&raw, 2000).await.expect("RAW"),
        vec![0, 0xEB, 0x03, 1, 2, 3]
    );
    assert_eq!(
        next_datagram(&raw, 2000).await.expect("REL"),
        vec![1, 1, 0, 0, 0, 9, 0, 8, 7]
    );
    // ACK it, so its retransmit does not interleave with what follows.
    raw.send_to(&encode_ack(2), addr).await.unwrap();
    let mut expected = vec![0, 0x4D, 0x04];
    expected.extend_from_slice(&at_budget);
    let d = next_datagram(&raw, 2000).await.expect("RAW at the budget");
    assert_eq!(d.len(), DEFAULT_MAX_DATAGRAM_BYTES);
    assert_eq!(d, expected, "exactly at the budget: still one RAW datagram");

    // One byte more: two FRAG datagrams of message 0, the RAW body
    // (op + payload) cut at 1467 bytes.
    let mut over = at_budget.clone();
    over.push(0xA5);
    out_tx
        .send(vec![FrameBody::new(1101, Bytes::from(over.clone()))])
        .await
        .unwrap();
    let mut body = vec![0x4D, 0x04];
    body.extend_from_slice(&over);
    let chunk = DEFAULT_MAX_DATAGRAM_BYTES - 5;
    let first = next_datagram(&raw, 2000).await.expect("fragment 0");
    let second = next_datagram(&raw, 2000).await.expect("fragment 1");
    assert_eq!(&first[..5], &[KIND_FRAG, 0, 0, 0, 2]);
    assert_eq!(&first[5..], &body[..chunk]);
    assert_eq!(&second[..5], &[KIND_FRAG, 0, 0, 1, 2]);
    assert_eq!(&second[5..], &body[chunk..]);
    assert_eq!(next_datagram(&raw, 200).await, None, "nothing else");
}

/// The control band never fragments: a control frame over the budget is
/// undeliverable, so it ends the session (through the actor's mailbox,
/// the reliable band's death path) — no FRAG datagram, no REL datagram,
/// and no seq spent on it. The control frames before it are untouched.
#[tokio::test]
async fn the_control_band_is_never_fragmented() {
    let cfg = UdpTransportConfig {
        max_datagram_bytes: 40,
        ..Default::default()
    };
    let (raw, _addr, out_tx, mut in_rx) = raw_session(cfg).await;

    out_tx
        .send(vec![FrameBody::new(
            gsb_protocol::op::base::HEARTBEAT_ACK,
            Bytes::from_static(&[4, 2]),
        )])
        .await
        .unwrap();
    assert_eq!(
        next_datagram(&raw, 2000).await.expect("REL"),
        vec![1, 1, 0, 0, 0, 8, 0, 4, 2]
    );
    // 5 + 2 + 34 = 41 bytes: one over the budget.
    out_tx
        .send(vec![FrameBody::new(
            gsb_protocol::op::base::ERROR,
            Bytes::from(vec![0xEEu8; 34]),
        )])
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(3), in_rx.recv())
        .await
        .expect("the actor must be told at once")
        .expect("an item");
    match closed {
        ConnIn::ServerClosed { cause, reason } => {
            assert_eq!(cause, ServerClose::RelDead);
            assert!(reason.contains("never fragmented"), "{reason}");
        }
        other => panic!("expected ServerClosed, got {other:?}"),
    }
    // The only datagram after the first REL is its retransmit (seq 1,
    // same bytes): nothing of the oversized frame reached the wire.
    while let Some(d) = next_datagram(&raw, 300).await {
        assert_eq!(d, vec![1, 1, 0, 0, 0, 8, 0, 4, 2], "only seq 1 again");
    }
}

/// The writer's losses reach the collector as it ends (BACKLOG B58): a
/// game frame past the fragmentation ceiling (dropped unsent), and the
/// unacknowledged control frame the band's death abandons.
#[tokio::test]
async fn the_writers_losses_are_sent_when_it_ends() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let cfg = UdpTransportConfig {
        max_datagram_bytes: 40,
        metrics: Some(tx),
        ..Default::default()
    };
    let (raw, _addr, out_tx, mut in_rx) = raw_session(cfg).await;
    let ack = FrameBody::new(
        gsb_protocol::op::base::HEARTBEAT_ACK,
        Bytes::from_static(&[4, 2]),
    );
    out_tx.send(vec![ack]).await.unwrap();
    assert!(next_datagram(&raw, 2000).await.is_some(), "REL seq 1");
    // 16 fragments of at most 35 bytes cannot carry 2000.
    out_tx
        .send(vec![FrameBody::new(1000, Bytes::from(vec![7u8; 2000]))])
        .await
        .unwrap();
    // A control frame over the budget kills the band (seq 1 unacked).
    out_tx
        .send(vec![FrameBody::new(
            gsb_protocol::op::base::ERROR,
            Bytes::from(vec![0xEEu8; 34]),
        )])
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), in_rx.recv()).await;
    drop(out_tx);
    let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("the writer's sample in time");
    let Some(gsb_core::metrics::MetricsEvent::Transport(t)) = got else {
        panic!("a transport sample: {got:?}");
    };
    assert_eq!(t.udp_frames_dropped_oversized, 1, "the 2000-byte frame");
    assert_eq!(t.udp_control_frames_abandoned, 1, "seq 1, never acked");
}
