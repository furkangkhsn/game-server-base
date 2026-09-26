//! `ServerHandle::stop` on the WebSocket door, end to end (BACKLOG B24):
//! the stop notice (`ERROR` 14) goes out first, then the door's close
//! frame with status 1001 "Going Away" — not the empty close frame a
//! client reads as 1005 ("no status") — then the end of the stream.
//!
//! A raw RFC 6455 client of its own (masked binary messages, one game
//! frame each): what this suite pins is the close frame's exact bytes,
//! below any client library's close handling.

use std::net::SocketAddr;
use std::time::Duration;

use gsb_client::session::{self, Credentials};
use gsb_protocol::base::{Error, ErrorCode};
use gsb_protocol::op::base::{ERROR, JOIN_ROOM_RESULT};
use gsb_server::{ListenerEntry, ListenerTransport};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const WINDOW: Duration = Duration::from_secs(5);

/// One server WS frame: `(opcode, payload)`.
async fn read_ws(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut head = [0u8; 2];
    tokio::time::timeout(WINDOW, s.read_exact(&mut head))
        .await
        .expect("a frame in time")
        .expect("frame head");
    assert_eq!(head[0] & 0x80, 0x80, "single-frame messages only");
    assert_eq!(head[1] & 0x80, 0, "the server never masks");
    let len = match head[1] & 0x7f {
        126 => s.read_u16().await.expect("len16") as usize,
        127 => s.read_u64().await.expect("len64") as usize,
        n => n as usize,
    };
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).await.expect("a whole WS frame");
    (head[0] & 0x0f, payload)
}

/// One masked FIN binary message carrying one game frame.
async fn write_game(s: &mut TcpStream, op: u16, payload: &[u8]) {
    let bytes = gsb_client::frame::encode(op, payload);
    assert!(bytes.len() < 126, "short frames only");
    let key = [0x21, 0x43, 0x65, 0x87];
    let mut msg = vec![0x82, 0x80 | bytes.len() as u8];
    msg.extend_from_slice(&key);
    msg.extend(bytes.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
    s.write_all(&msg).await.expect("write");
}

async fn upgrade(addr: SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.expect("ws tcp");
    let req = format!(
        "GET /gsb HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.expect("upgrade request");
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(s.read_u8().await.expect("upgrade response"));
    }
    assert!(head.starts_with(b"HTTP/1.1 101"), "the door upgrades");
    s
}

#[tokio::test]
async fn a_stopped_server_closes_the_websocket_with_1001_going_away() {
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![ListenerEntry {
            transport: ListenerTransport::Ws,
            bind: "127.0.0.1:0".into(),
            tls_cert: None,
            tls_key: None,
        }]),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let mut s = upgrade(handle.addrs[0]).await;
    for f in [
        session::auth_req(&Credentials::named("going-away")),
        session::join_req(1),
    ] {
        write_game(&mut s, f.op, &f.payload).await;
    }
    loop {
        let (opcode, payload) = read_ws(&mut s).await;
        assert_eq!(opcode, 0x2, "a binary message before the join result");
        if u16::from_le_bytes([payload[4], payload[5]]) == JOIN_ROOM_RESULT {
            break;
        }
    }

    tokio::time::timeout(WINDOW, handle.stop())
        .await
        .expect("stop() completes");

    // Game frames still in flight, then the notice, then the close —
    // nothing after the notice but the close.
    let notice = loop {
        let (opcode, payload) = read_ws(&mut s).await;
        assert_eq!(opcode, 0x2, "a binary message before the notice");
        if u16::from_le_bytes([payload[4], payload[5]]) == ERROR {
            break Error::decode(&payload[6..]).expect("decodes");
        }
    };
    assert_eq!(notice.code(), ErrorCode::ServerStopping);
    let (opcode, payload) = read_ws(&mut s).await;
    assert_eq!(opcode, 0x8, "the close frame follows the notice");
    assert_eq!(payload, [0x03, 0xE9], "status 1001, no reason");
    // The client answers the close as RFC 6455 §5.5.1 asks; the server
    // closed first, so it echoes nothing — the stream just ends.
    let key = [0x21, 0x43, 0x65, 0x87];
    let mut answer = vec![0x88, 0x82];
    answer.extend_from_slice(&key);
    answer.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
    s.write_all(&answer).await.expect("close answer");
    let mut rest = [0u8; 1];
    match tokio::time::timeout(WINDOW, s.read(&mut rest)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        other => panic!("expected the end of the stream, got {other:?}"),
    }
}
