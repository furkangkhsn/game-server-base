//! What counts as the server's accept: besides the `ACK{1}`, any
//! session datagram — and the one that confirms the session is delivered
//! like any other. A fake server (a bare socket) plays the server so the
//! first answer to the proof can be chosen. Child of `handshake`.

use super::*;

/// A fake server that answers the proof with `first` (its accept lost,
/// the session already talking) and returns the confirmed client. The
/// fake's socket lives until the returned handle is awaited.
async fn confirmed_by(first: Vec<u8>) -> (UdpClient, tokio::task::JoinHandle<UdpSocket>) {
    let server = UdpSocket::bind("127.0.0.1:0").await.expect("fake server");
    let addr = server.local_addr().unwrap();
    let fake = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let (_, from) = server.recv_from(&mut buf).await.unwrap();
        let nonce = u64::from_le_bytes(buf[1..9].try_into().unwrap());
        let proof = encode_hello(nonce, 0xC0FFEE);
        server.send_to(&proof, from).await.unwrap();
        let (n, _) = server.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &proof[..], "the proof carries the cookie");
        server.send_to(&first, from).await.unwrap();
        server
    });
    let within = Duration::from_secs(2);
    let c = tokio::time::timeout(within * 2, UdpClient::connect_within(addr, within))
        .await
        .expect("bounded")
        .expect("a session datagram is evidence of the session");
    assert!(c.is_established());
    (c, fake)
}

/// The first answer to the proof is a CONTROL frame: the same evidence
/// as the accept, and the frame is DELIVERED, not swallowed by the
/// handshake.
#[tokio::test]
async fn a_control_frame_in_place_of_the_accept_confirms_and_is_delivered() {
    let frame = FrameBody::new(gsb_protocol::op::base::ERROR, Bytes::from_static(&[3, 1]));
    let (mut c, fake) = confirmed_by(encode_rel(1, &frame)).await;
    let f = c
        .recv_frame(Duration::from_millis(500))
        .await
        .expect("recv")
        .expect("the frame that confirmed the session is delivered");
    assert_eq!(f.op, gsb_protocol::op::base::ERROR);
    assert_eq!(f.payload.as_ref(), &[3u8, 1]);
    drop(fake.await.expect("fake server"));
}

/// The same for a GAME-band frame: it waits in the client's RAW slot and
/// the first `recv_frame` returns it without needing another datagram.
#[tokio::test]
async fn a_game_frame_in_place_of_the_accept_confirms_and_is_delivered() {
    let frame = FrameBody::new(1003, Bytes::from_static(&[7, 7]));
    let (mut c, fake) = confirmed_by(encode_raw(&frame)).await;
    let f = c
        .recv_frame(Duration::from_millis(300))
        .await
        .expect("recv")
        .expect("the game frame that confirmed the session is delivered");
    assert_eq!((f.op, f.payload.as_ref()), (1003, &[7u8, 7][..]));
    drop(fake.await.expect("fake server"));
}
