//! The WebSocket close code by how the session ended, end to end
//! (BACKLOG B30): a session the server closes for a protocol-policy
//! verdict — here the violation budget — gets its `ERROR` 9 notice
//! exactly as before, then a close frame with status 1008 "Policy
//! Violation" instead of 1001 "Going Away" (the stop keeps 1001:
//! `ws_going_away.rs`). The reason travels from the connection actor to
//! the door through the endpoint's end notice, wired by the accept loop.
//!
//! A raw RFC 6455 client (masked binary messages, one game frame each),
//! as in `ws_going_away.rs`: the close frame's exact bytes are the point.

use std::net::SocketAddr;
use std::time::Duration;

use gsb_client::session::{self, Credentials};
use gsb_protocol::base::{Error, ErrorCode};
use gsb_protocol::op::base::{AUTH_RESULT, ERROR};
use gsb_server::{ListenerEntry, ListenerTransport};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const WINDOW: Duration = Duration::from_secs(5);

/// A base-band opcode no message is registered under: each one is a hard
/// protocol violation (four of them exhaust the budget).
const UNDEFINED_BASE_OP: u16 = 99;

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

/// The game frame's opcode inside a binary message.
fn op_of(payload: &[u8]) -> u16 {
    u16::from_le_bytes([payload[4], payload[5]])
}

#[tokio::test]
async fn a_violation_budget_close_carries_1008_policy_violation() {
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
    let auth = session::auth_req(&Credentials::named("cheater"));
    write_game(&mut s, auth.op, &auth.payload).await;
    loop {
        let (opcode, payload) = read_ws(&mut s).await;
        assert_eq!(opcode, 0x2, "a binary message before the auth result");
        if op_of(&payload) == AUTH_RESULT {
            break;
        }
    }
    for _ in 0..4 {
        write_game(&mut s, UNDEFINED_BASE_OP, &[]).await;
    }

    // The answered violations, then the verdict's notice — as before B30.
    let notice = loop {
        let (opcode, payload) = read_ws(&mut s).await;
        assert_eq!(opcode, 0x2, "a binary message before the close");
        if op_of(&payload) == ERROR {
            let e = Error::decode(&payload[6..]).expect("decodes");
            if e.code() == ErrorCode::ServerClosed {
                break e;
            }
        }
    };
    assert!(notice.message.contains("violation"), "{}", notice.message);
    let (opcode, payload) = read_ws(&mut s).await;
    assert_eq!(opcode, 0x8, "the close frame follows the notice");
    assert_eq!(payload, [0x03, 0xF0], "status 1008, no reason");

    tokio::time::timeout(WINDOW, handle.stop())
        .await
        .expect("stop() completes");
}
