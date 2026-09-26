//! What the reader refuses — the frame guard (from the header, before
//! the payload), every protocol violation, EOF inside a message — and
//! cancel safety: a read or a send cancelled mid-message loses nothing.

use std::io::ErrorKind;

use super::*;

/// The guard is the frame body (as on a stream door); the message is its
/// 4-byte envelope more. At the guard: accepted. One byte over: refused
/// from the header alone — its payload never sent, never awaited.
#[tokio::test]
async fn the_guard_refuses_a_message_from_its_header() {
    let (mut conn, mut peer) = pair(16);
    peer.game(1, &[7u8; 14]).await; // body 16: at the guard
    let f = frame(&mut conn).await;
    assert_eq!(f.payload.len(), 14);
    peer.send(&[0x80 | OP_BIN, 21]).await; // a 21-byte message: over
    let e = tokio::time::timeout(Duration::from_secs(1), conn.recv(W))
        .await
        .expect("refused without waiting for the payload")
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidData);
}

/// Fragments add up: a message whose fragments together pass the guard
/// is refused at the fragment that passes it, from its header.
#[tokio::test]
async fn reassembly_is_held_to_the_guard() {
    let (mut conn, mut peer) = pair(16);
    peer.send(&server_frame(false, OP_BIN, &[0u8; 12])).await;
    peer.send(&[OP_CONT | 0x80, 9]).await; // 12 + 9 > 20
    let e = tokio::time::timeout(Duration::from_secs(1), conn.recv(W))
        .await
        .expect("refused without waiting for the payload")
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidData);
}

/// The default guard is the stream doors' 4 MiB.
#[tokio::test]
async fn the_default_guard_is_four_mebibytes() {
    let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    let mut head = vec![0x80 | OP_BIN, 127];
    head.extend_from_slice(&(4u64 * 1024 * 1024 + 5).to_be_bytes());
    peer.send(&head).await;
    let e = conn.recv(W).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidData);
}

/// Every violation is `InvalidData`, and the reader stays failed.
#[tokio::test]
async fn protocol_violations_are_refused() {
    let one = encode(1, b"x");
    let masked = {
        let mut f = server_frame(true, OP_BIN, &one);
        f[1] |= 0x80;
        f.splice(2..2, [0u8; 4]);
        f
    };
    let rsv = {
        let mut f = server_frame(true, OP_BIN, &one);
        f[0] |= 0x40;
        f
    };
    let two = [encode(1, b"a"), encode(2, b"b")].concat();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("masked server frame", masked),
        ("RSV bit", rsv),
        ("text message", server_frame(true, OP_TEXT, b"hi")),
        ("unknown opcode", server_frame(true, 0x3, &one)),
        ("fragmented ping", server_frame(false, OP_PING, b"p")),
        ("long ping", server_frame(true, OP_PING, &[0u8; 126])),
        ("stray continuation", server_frame(true, OP_CONT, &one)),
        (
            "message inside a message",
            [
                server_frame(false, OP_BIN, &one[..3]),
                server_frame(true, OP_BIN, &one),
            ]
            .concat(),
        ),
        (
            "two frames in one message",
            server_frame(true, OP_BIN, &two),
        ),
        ("short envelope", server_frame(true, OP_BIN, &[1, 0, 0])),
        ("no opcode", server_frame(true, OP_BIN, &[1, 0, 0, 0, 9])),
        (
            "envelope over-declares",
            server_frame(true, OP_BIN, &one[..6]),
        ),
    ];
    for (what, bytes) in cases {
        let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
        peer.send(&bytes).await;
        let e = conn.recv(W).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidData, "{what}: {e}");
        peer.game(1, b"after").await;
        let again = conn.recv(W).await.unwrap_err();
        assert_eq!(again.kind(), ErrorKind::InvalidData, "{what}: stays failed");
    }
}

/// A stream that ends inside a frame, or inside a fragmented message, is
/// `UnexpectedEof` — not a clean end.
#[tokio::test]
async fn eof_inside_a_message_is_unexpected_eof() {
    let cut_frame = server_frame(true, OP_BIN, &encode(1, b"abcdef"))[..5].to_vec();
    let open_message = server_frame(false, OP_BIN, &encode(1, b"abcdef")[..4]);
    for bytes in [cut_frame, open_message] {
        let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
        peer.send(&bytes).await;
        drop(peer.wr);
        let e = conn.recv(W).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnexpectedEof);
    }
}

/// THE cancel-safety rule (the B19 bug class): windows that end inside a
/// frame header, inside a payload and between fragments lose nothing —
/// the message comes back whole, and the next one after it.
#[tokio::test]
async fn a_read_cancelled_mid_message_loses_nothing() {
    let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    let msg = encode(77, &(0u8..200).collect::<Vec<_>>());
    let first = server_frame(false, OP_BIN, &msg[..150]); // 16-bit length form
    let second = server_frame(true, OP_CONT, &msg[150..]);
    // Windows end inside the header, twice inside the payload (the
    // second one after part of it was read in that very window), at the
    // fragment boundary and inside the next header.
    let cuts: [&[u8]; 6] = [
        &first[..3],
        &first[3..60],
        &first[60..100],
        &first[100..],
        &second[..1],
        &second[1..],
    ];
    for cut in &cuts[..5] {
        peer.send(cut).await;
        let quiet = conn.recv(Duration::from_millis(30)).await.unwrap();
        assert!(matches!(quiet, Recv::Quiet), "no whole message yet");
    }
    peer.send(cuts[5]).await;
    peer.game(78, b"next").await;
    let f = frame(&mut conn).await;
    assert_eq!((f.op, f.payload.len()), (77, 200));
    assert_eq!(&f.payload[..], &(0u8..200).collect::<Vec<_>>()[..]);
    let g = frame(&mut conn).await;
    assert_eq!((g.op, &g.payload[..]), (78, &b"next"[..]));
}

/// A send cancelled while the socket is full leaves the rest of its
/// frame queued: the next send writes it first, and both messages reach
/// the server whole.
#[tokio::test]
async fn a_send_cancelled_mid_frame_loses_nothing() {
    let (client_rd, _server_wr) = duplex(64);
    let (server_rd, client_wr) = duplex(64); // a tiny pipe: writes block
    let mut conn = super::super::conn(
        Box::new(client_rd),
        Box::new(client_wr),
        BytesMut::new(),
        DEFAULT_MAX_FRAME_BYTES,
    );
    let big = vec![0xabu8; 1000];
    let cancelled = tokio::time::timeout(Duration::from_millis(30), conn.send(1, &big)).await;
    assert!(cancelled.is_err(), "the pipe is full: the send must block");
    let reader = tokio::spawn(async move {
        let mut peer = Peer {
            rd: server_rd,
            wr: duplex(1).0,
        };
        (peer.read().await, peer.read().await)
    });
    conn.send(2, b"second").await.unwrap();
    let (a, b) = reader.await.unwrap();
    assert_eq!((a.opcode, a.payload), (OP_BIN, encode(1, &big)));
    assert_eq!((b.opcode, b.payload), (OP_BIN, encode(2, b"second")));
}
