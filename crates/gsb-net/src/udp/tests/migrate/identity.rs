//! Migration off is today's door, byte for byte (BACKLOG B3 — the way
//! round 3 pinned its non-reporting client): a scripted session on a raw
//! socket — handshake, a control frame and a game frame from the
//! session, a control frame from the client and its ACK — read every
//! server → client byte. The proof with the capability byte (a new
//! client) on a door with migration off, the proof without it (an older
//! client) on a door with migration off, and the same older client on a
//! door with migration on: the same bytes, and the bytes of the wire as
//! it was.

use super::*;

/// The scripted session: every datagram the client socket received.
async fn script(on: bool, caps: u8) -> Vec<Vec<u8>> {
    let cfg = UdpTransportConfig {
        migration: on,
        ..Default::default()
    };
    let (listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let raw = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut seen = Vec::new();
    let mut buf = vec![0u8; 2048];
    let nonce = 0x1D_u64;
    raw.send_to(&encode_hello(nonce, 0), addr).await.unwrap();
    let next = async |raw: &UdpSocket, buf: &mut Vec<u8>| {
        let (n, _) = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(buf))
            .await
            .expect("a datagram")
            .unwrap();
        buf[..n].to_vec()
    };
    let challenge = next(&raw, &mut buf).await;
    let cookie = u64_at(&challenge, 9).unwrap();
    raw.send_to(&encode_proof(nonce, cookie, caps), addr)
        .await
        .unwrap();
    seen.push(next(&raw, &mut buf).await);
    let mut ep = eps.recv().await.unwrap();
    let (in_tx, _in_rx) = ep.take_inbox(16);
    let (out_tx, out_rx) = ep.take_outbox(16);
    let _pump = ep.start_pump(
        ConnectionId(1),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    let control = FrameBody::new(op::HEARTBEAT_ACK, Bytes::from_static(b"c"));
    out_tx.send(vec![control]).await.unwrap();
    seen.push(next(&raw, &mut buf).await);
    let game = FrameBody::new(1003, Bytes::from_static(b"g"));
    out_tx.send(vec![game]).await.unwrap();
    seen.push(next(&raw, &mut buf).await);
    let hb = FrameBody::new(op::HEARTBEAT, Bytes::from_static(b"h"));
    raw.send_to(&encode_rel(1, &hb), addr).await.unwrap();
    seen.push(next(&raw, &mut buf).await);
    listener.close();
    seen
}

#[tokio::test]
async fn migration_off_is_the_old_wire_byte_for_byte() {
    let expect = vec![
        encode_ack(1),
        encode_rel(
            1,
            &FrameBody::new(op::HEARTBEAT_ACK, Bytes::from_static(b"c")),
        ),
        encode_raw(&FrameBody::new(1003, Bytes::from_static(b"g"))),
        encode_ack(2),
    ];
    assert_eq!(expect[0], [KIND_ACK, 1, 0, 0, 0], "the old accept");
    for (on, caps) in [(false, CAP_CID), (false, 0), (true, 0)] {
        assert_eq!(script(on, caps).await, expect, "door {on}, caps {caps}");
    }
}
