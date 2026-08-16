//! End-to-end test: an in-process gsb server on an ephemeral port, a real
//! TCP client that authenticates, joins room 1, issues a move, and asserts
//! that it observes its entity's position change in the world snapshots.

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

    // The room ships self-contained world snapshots (one per change, plus
    // a low-rate keep-alive). Success = we first observe our entity, then
    // observe its position change (the move propagated: action → ingest →
    // movement system → snapshot → writer pump).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut my_entity: u64 = 0;
    let mut move_sent = false;
    let mut first_pos: Option<(i32, i32)> = None;

    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out (move_sent={move_sent})"));
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
                // Force movement so the room re-emits a snapshot.
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
            gsb_game::op::WORLD_SNAPSHOT => {
                let m: gsb_game::game::WorldSnapshot =
                    gsb_game::game::WorldSnapshot::decode(&payload[..]).unwrap();
                assert!(
                    m.sequence > 0,
                    "snapshot sequence must be monotonic (> 0)"
                );
                let Some(rec) = m.entities.iter().find(|e| e.entity == my_entity) else {
                    continue; // snapshot that arrived before the join result
                };
                let pos = (rec.x, rec.y);
                match first_pos {
                    None => first_pos = Some(pos),
                    Some(first) => {
                        if pos != first {
                            break; // moved: the full path is proven
                        }
                        // Still at the spawn position: keep reading.
                    }
                }
            }
            other => {
                panic!("unexpected op {other} in e2e handshake");
            }
        }
    }

    assert!(move_sent, "must have been able to send MOVE_TO");
    handle.stop().await;
}
