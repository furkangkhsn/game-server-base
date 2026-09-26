//! [`ws_stream`]: the WebSocket door over a TCP stream the caller
//! connected, against a scripted server on a real socket — the request
//! names the caller's host, the bytes behind the 101 are the first
//! frame, and the client's frames go out as masked binary messages.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::Recv;
use crate::frame::encode;
use crate::ws::{OP_BIN, accept_key};

const W: Duration = Duration::from_secs(5);

/// Accept one connection, answer its upgrade with a 101 and one frame
/// (op 42, "early") in the same write, then read the client's first
/// message. Returns the request head and that message, unmasked.
async fn scripted(listener: TcpListener) -> (String, Vec<u8>) {
    let (mut sock, _) = listener.accept().await.expect("accept");
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(sock.read_u8().await.expect("request head"));
    }
    let head = String::from_utf8(head).expect("UTF-8 head");
    let key = head
        .lines()
        .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
        .expect("a key");
    let mut out = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(key)
    )
    .into_bytes();
    let early = encode(42, b"early");
    out.extend([0x80 | OP_BIN, early.len() as u8]);
    out.extend(&early);
    sock.write_all(&out).await.expect("answer");
    let mut h = [0u8; 6];
    sock.read_exact(&mut h).await.expect("frame header");
    assert_eq!(h[0], 0x80 | OP_BIN, "one FIN binary message");
    assert_ne!(h[1] & 0x80, 0, "masked");
    let mut body = vec![0u8; usize::from(h[1] & 0x7f)];
    sock.read_exact(&mut body).await.expect("frame body");
    for (i, b) in body.iter_mut().enumerate() {
        *b ^= h[2 + (i & 3)];
    }
    (head, body)
}

#[tokio::test]
async fn ws_stream_upgrades_the_callers_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(scripted(listener));
    let stream = TcpStream::connect(addr).await.expect("connect");
    let mut conn = ws_stream(stream, "gsb.test:9").await.expect("upgraded");
    assert!(conn.is_ws());
    match conn.recv(W).await.expect("no error") {
        Recv::Frame(f) => assert_eq!((f.op, &f.payload[..]), (42, &b"early"[..])),
        other => panic!("want the early frame, got {other:?}"),
    }
    conn.send(7, b"hello").await.expect("send");
    let (head, body) = server.await.expect("server");
    assert!(head.starts_with("GET / HTTP/1.1\r\n"), "{head}");
    assert!(head.contains("\r\nHost: gsb.test:9\r\n"), "{head}");
    assert_eq!(body, encode(7, b"hello"));
}
