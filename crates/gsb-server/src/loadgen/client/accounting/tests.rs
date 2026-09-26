//! The WebSocket byte accounting (BACKLOG B29): the sizes RFC 6455 puts
//! around a frame, and a whole `run_client` session against a scripted
//! WebSocket peer that counts the data messages that really crossed the
//! socket — the client's `bytes_out` / `bytes_in` are exactly what the
//! peer read / wrote, pings and pongs excluded, a plain and a flooding
//! client alike.

use std::time::{Duration, Instant};

use gsb_client::frame::encode;
use gsb_client::ws::{OP_BIN, OP_PING, OP_PONG, accept_key};
use gsb_protocol::base::{JoinRoomResult, LeaveRoomResult};
use gsb_protocol::op;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::client::{ClientParams, run_client};

/// The header sizes at each length boundary, both ways.
#[test]
fn a_ws_message_is_its_header_the_mask_and_the_frame() {
    // Bodies (6 + payload) of 125, 126, 65535 and 65536 bytes.
    for (payload, header) in [(0, 2), (119, 2), (120, 4), (65529, 4), (65530, 10)] {
        let body = (6 + payload) as u64;
        assert_eq!(
            ws_message_bytes(Dir::In, payload),
            header + body,
            "{payload}"
        );
        assert_eq!(ws_message_bytes(Dir::Out, payload), header + 4 + body);
    }
}

/// One server frame: FIN, unmasked, the minimal length form.
fn server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => out.push(n as u8),
        n if n <= 0xffff => {
            out.push(126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// The client's next frame: `(opcode, unmasked payload, bytes on the
/// wire)`, or `None` at its EOF.
async fn read_frame(sock: &mut TcpStream) -> Option<(u8, Vec<u8>, u64)> {
    let mut h = [0u8; 2];
    sock.read_exact(&mut h).await.ok()?;
    assert_ne!(h[1] & 0x80, 0, "client frames are masked");
    let (len, ext) = match h[1] & 0x7f {
        126 => (u64::from(sock.read_u16().await.ok()?), 2),
        127 => (sock.read_u64().await.ok()?, 8),
        n => (u64::from(n), 0),
    };
    let mut key = [0u8; 4];
    sock.read_exact(&mut key).await.ok()?;
    let mut payload = vec![0u8; len as usize];
    sock.read_exact(&mut payload).await.ok()?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= key[i & 3];
    }
    Some((h[0] & 0x0f, payload, 2 + ext + 4 + len))
}

/// What the peer counted: `(data bytes read, data bytes written, pongs)`.
type Tally = (u64, u64, u32);

/// Upgrade one connection, then: a ping and the join result (plus three
/// frames around the two extended-length boundaries) after the JOIN, the
/// leave result after the LEAVE; read until the client drops.
async fn peer(listener: TcpListener) -> Tally {
    let (mut sock, _) = listener.accept().await.expect("accept");
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(sock.read_u8().await.expect("request head"));
    }
    let head = String::from_utf8(head).expect("UTF-8");
    let key = head
        .lines()
        .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
        .expect("a key");
    let answer = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(key)
    );
    sock.write_all(answer.as_bytes()).await.expect("101");
    let (mut read, mut sent, mut pongs) = (0u64, 0u64, 0u32);
    while let Some((opcode, body, n)) = read_frame(&mut sock).await {
        if opcode == OP_PONG {
            pongs += 1;
            continue;
        }
        assert_eq!(opcode, OP_BIN, "data travels as binary messages");
        read += n;
        let op = u16::from_le_bytes([body[4], body[5]]);
        let replies: Vec<(u16, Vec<u8>)> = match op {
            op::base::JOIN_ROOM_REQ => {
                sock.write_all(&server_frame(OP_PING, b"hi"))
                    .await
                    .expect("ping");
                let joined = JoinRoomResult { entity: 42 }.encode_to_vec();
                let mut r = vec![(op::base::JOIN_ROOM_RESULT, joined)];
                for len in [119usize, 120, 70_000] {
                    r.push((op::base::HEARTBEAT_ACK, vec![1; len]));
                }
                r
            }
            op::base::LEAVE_ROOM_REQ => {
                let left = LeaveRoomResult::default().encode_to_vec();
                vec![(op::base::LEAVE_ROOM_RESULT, left)]
            }
            _ => Vec::new(),
        };
        for (op, payload) in replies {
            let f = server_frame(OP_BIN, &encode(op, &payload));
            sent += f.len() as u64;
            sock.write_all(&f).await.expect("peer write");
        }
    }
    (read, sent, pongs)
}

/// A plain and a flooding client: every data message on the wire is
/// counted at its size, and nothing else is.
#[tokio::test]
async fn ws_bytes_are_the_messages_on_the_wire() {
    for flood in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let peer = tokio::spawn(peer(listener));
        let args = crate::Args::defaults();
        let p = ClientParams {
            tls: None,
            addr,
            room: 1,
            move_ms: Duration::from_millis(100),
            stagger_ms: 0.0,
            bot: crate::bot::bot_for(&args),
            deadline: Instant::now() + Duration::from_millis(800),
            flood,
            kind: crate::Transport::Ws,
            capture: None,
            stall: None,
        };
        let rep = run_client(3, p).await;
        let (read, sent, pongs) = peer.await.expect("peer");
        assert!(rep.joined && rep.left, "a whole session (flood {flood})");
        assert!(rep.moves > 0, "inputs went out (flood {flood})");
        assert_eq!(pongs, 1, "the ping was answered (flood {flood})");
        assert_eq!(
            rep.bytes_out, read,
            "client out = peer read (flood {flood})"
        );
        assert_eq!(rep.bytes_in, sent, "client in = peer wrote (flood {flood})");
    }
}
