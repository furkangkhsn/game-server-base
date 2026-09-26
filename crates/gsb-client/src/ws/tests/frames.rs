//! Data both ways: a frame goes out as ONE masked binary message (a
//! fresh key each), server messages in every length form come back, a
//! fragmented message is reassembled around a ping, and a ping is
//! answered — at once inside `recv`, ahead of the next frame when split.

use super::*;

/// `send` and `send_batch`: one masked FIN binary message per frame,
/// carrying exactly the stream-wire bytes; every frame its own key.
#[tokio::test]
async fn each_frame_goes_out_as_one_masked_binary_message() {
    let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    conn.send(7, b"hello").await.unwrap();
    conn.send_batch(&[
        gsb_protocol::FrameBody::new(1, vec![1u8, 2]),
        gsb_protocol::FrameBody::new(2, vec![9u8; 300]),
    ])
    .await
    .unwrap();
    conn.send(3, &vec![5u8; 70_000]).await.unwrap();
    let want = [
        encode(7, b"hello"),
        encode(1, &[1, 2]),
        encode(2, &[9; 300]),
        encode(3, &vec![5; 70_000]),
    ];
    let mut keys = std::collections::HashSet::new();
    for w in want {
        let f = peer.read().await;
        assert!(f.masked, "RFC 6455 §5.3: every client frame is masked");
        assert!(f.fin);
        assert_eq!(f.opcode, OP_BIN);
        assert_eq!(f.payload, w, "the frame's stream-wire bytes, unmasked");
        keys.insert(f.key);
    }
    assert_eq!(keys.len(), 4, "a fresh key per frame");
    peer.silent().await;
}

/// Server messages in the 7-bit, 16-bit and 64-bit length forms each
/// come back as their frame, in order.
#[tokio::test]
async fn server_messages_of_every_length_form_come_back() {
    let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    let sizes = [0usize, 100, 300, 70_000];
    for (i, n) in sizes.iter().enumerate() {
        peer.game(i as u16 + 10, &vec![i as u8; *n]).await;
    }
    for (i, n) in sizes.iter().enumerate() {
        let f = frame(&mut conn).await;
        assert_eq!(f.op, i as u16 + 10);
        assert_eq!(f.payload.len(), *n);
        assert!(f.payload.iter().all(|b| *b == i as u8));
    }
}

/// One message in three fragments with a ping between them (§5.4:
/// control frames may interleave) is ONE frame; the ping is answered
/// with a masked pong carrying its payload.
#[tokio::test]
async fn a_fragmented_message_is_reassembled_around_a_ping() {
    let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    let msg = encode(5, &(0u8..50).collect::<Vec<_>>());
    peer.send(&server_frame(false, OP_BIN, &msg[..10])).await;
    peer.send(&server_frame(true, OP_PING, b"p1")).await;
    peer.send(&server_frame(false, OP_CONT, &msg[10..30])).await;
    peer.send(&server_frame(true, OP_CONT, &msg[30..])).await;
    peer.game(6, b"after").await;
    let f = frame(&mut conn).await;
    assert_eq!(f.op, 5);
    assert_eq!(&f.payload[..], &(0u8..50).collect::<Vec<_>>()[..]);
    let g = frame(&mut conn).await;
    assert_eq!((g.op, &g.payload[..]), (6, &b"after"[..]));
    let pong = peer.read().await;
    assert!(pong.masked && pong.fin);
    assert_eq!((pong.opcode, &pong.payload[..]), (OP_PONG, &b"p1"[..]));
}

/// `recv` answers a ping within its window even when no frame follows:
/// the window ends `Quiet`, the pong is already on the wire. A pong from
/// the server is ignored.
#[tokio::test]
async fn recv_answers_a_ping_even_when_no_frame_follows() {
    let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    peer.send(&server_frame(true, OP_PONG, b"unsolicited"))
        .await;
    peer.send(&server_frame(true, OP_PING, b"are you there"))
        .await;
    let quiet = conn.recv(Duration::from_millis(100)).await.unwrap();
    assert!(matches!(quiet, Recv::Quiet), "{quiet:?}");
    let pong = peer.read().await;
    assert_eq!(pong.opcode, OP_PONG);
    assert_eq!(pong.payload, b"are you there");
    peer.silent().await;
}

/// Split halves: the read half queues the pong, the write half sends it
/// AHEAD of its next frame.
#[tokio::test]
async fn split_halves_send_the_pong_ahead_of_the_next_frame() {
    let (conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    let Ok((mut rx, mut tx)) = conn.into_split() else {
        panic!("a WebSocket splits");
    };
    peer.send(&server_frame(true, OP_PING, b"split")).await;
    peer.game(8, b"data").await;
    let f = rx.next().await.unwrap().expect("the frame");
    assert_eq!((f.op, &f.payload[..]), (8, &b"data"[..]));
    tx.send(9, b"reply").await.unwrap();
    let first = peer.read().await;
    assert_eq!((first.opcode, &first.payload[..]), (OP_PONG, &b"split"[..]));
    let second = peer.read().await;
    assert_eq!(
        (second.opcode, second.payload),
        (OP_BIN, encode(9, b"reply"))
    );
}

/// `ws_frame` writes any frame as given (masked): a text message, a
/// fragment, a close — the raw door the test rigs need. On a byte
/// stream it is refused.
#[tokio::test]
async fn ws_frame_writes_any_frame_as_given() {
    let (conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    let Ok((_rx, mut tx)) = conn.into_split() else {
        panic!("a WebSocket splits");
    };
    tx.ws_frame(true, OP_TEXT, b"hi").await.unwrap();
    tx.ws_frame(false, OP_BIN, b"frag").await.unwrap();
    let text = peer.read().await;
    assert!(text.masked && text.fin);
    assert_eq!((text.opcode, &text.payload[..]), (OP_TEXT, &b"hi"[..]));
    let frag = peer.read().await;
    assert!(frag.masked && !frag.fin);
    assert_eq!((frag.opcode, &frag.payload[..]), (OP_BIN, &b"frag"[..]));

    let (a, _b) = duplex(64);
    let mut plain = crate::frame::FrameTx::new(a);
    let e = plain.ws_frame(true, OP_TEXT, b"hi").await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
}
