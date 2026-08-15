//! End-to-end test: an in-process gsb server on an ephemeral port, a real
//! TCP client that authenticates, joins room 1, issues a move, and asserts
//! that it receives its entity's state snapshot.

use std::time::{Duration, Instant};

use gsb_protocol::base::{Auth, AuthResult, Error, JoinRoom, JoinRoomResult};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn frame(op: u16, msg: &impl Message) -> Vec<u8> {
    let payload = msg.encode_to_vec();
    let body = 2 + payload.len();
    let mut out = Vec::with_capacity(4 + body);
    out.extend_from_slice(&(body as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

async fn read_frame(stream: &mut TcpStream) -> Option<(u16, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 4 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.ok()?;
    if body.len() < 2 {
        return None;
    }
    let op = u16::from_le_bytes([body[0], body[1]]);
    Some((op, body[2..].to_vec()))
}

#[tokio::test]
async fn client_joins_and_receives_snapshots() {
    // In-process server on an ephemeral port.
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let addr = handle.addr;

    let mut stream = TcpStream::connect(addr).await.expect("client connects");
    stream
        .write_all(&frame(
            gsb_protocol::op::base::AUTH_REQ,
            &Auth { name: "e2e".into() },
        ))
        .await
        .unwrap();
    stream
        .write_all(&frame(
            gsb_protocol::op::base::JOIN_ROOM_REQ,
            &JoinRoom { room_id: 1 },
        ))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut my_entity: u64 = 0;
    let mut move_sent = false;
    let mut saw_spawn = false;

    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out (spawn={saw_spawn})"));
        let (op, payload) = match tokio::time::timeout(remaining, read_frame(&mut stream)).await {
            Ok(Some(f)) => f,
            Ok(None) => panic!("server closed the connection"),
            Err(_) => panic!("timed out waiting for frames"),
        };

        match op {
            gsb_protocol::op::base::AUTH_RESULT => {
                let m: AuthResult = AuthResult::decode(&payload[..]).unwrap();
                assert!(m.ok, "auth must succeed");
            }
            gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                let m: JoinRoomResult = JoinRoomResult::decode(&payload[..]).unwrap();
                my_entity = m.entity;
                assert!(my_entity != 0, "entity id must be non-zero");
                // Force movement so the room broadcasts a fresh snapshot.
                stream
                    .write_all(&frame(
                        gsb_game::op::MOVE_TO,
                        &gsb_game::game::MoveTo { x: 10, y: 10 },
                    ))
                    .await
                    .unwrap();
                move_sent = true;
            }
            gsb_protocol::op::base::ERROR => {
                let m: Error = Error::decode(&payload[..]).unwrap();
                panic!("server error: code={} message={}", m.code, m.message);
            }
            gsb_game::op::ENTITY_SPAWNED => {
                saw_spawn = true;
            }
            gsb_game::op::ENTITY_STATE => {
                let m: gsb_game::game::EntityState =
                    gsb_game::game::EntityState::decode(&payload[..]).unwrap();
                // The state for our entity (after the join result) proves the
                // full path: action → ingest → movement system → broadcast.
                if my_entity != 0 {
                    assert_eq!(m.entity, my_entity);
                    assert!(m.version > 0, "entity must have moved (version > 0)");
                    break;
                }
                // A state that arrived before the join result: keep waiting.
            }
            other => {
                panic!("unexpected op {other} in e2e handshake");
            }
        }
    }

    assert!(saw_spawn, "must have seen ENTITY_SPAWNED");
    assert!(move_sent, "must have been able to send MOVE_TO");
    handle.stop().await;
}
