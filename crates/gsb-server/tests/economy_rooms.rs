//! The economy service answers through EVERY demo room build
//! (GAME-MODULE.md §6 decision 11).
//!
//! Every kit room forwards requests to the game (`Game::handle_request`),
//! so whether an `ECONOMY` request is served depends only on whether the
//! server attached the service to the room's game. The server used to
//! attach it to three of the six builds (open, sharded, sharded × spatial)
//! and the AOI, team and PVS rooms answered "economy service not
//! configured". This suite drives one real `ECONOMY` round trip over TCP
//! through each of the six builds and asserts the service's answer.

use std::time::{Duration, Instant};

use gsb_protocol::base::{Auth, AuthResult, Error, JoinRoom, JoinRoomResult, RpcRequest};
use gsb_server::{Config, Topology, Visibility};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Write one length-prefixed frame (`u32` body length, `u16` op, payload).
async fn write_frame(s: &mut TcpStream, op: u16, payload: &[u8]) {
    let mut out = Vec::with_capacity(6 + payload.len());
    out.extend_from_slice(&((2 + payload.len()) as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    s.write_all(&out).await.expect("write frame");
}

/// Read one frame, or `None` when nothing arrives within `window`.
async fn read_frame(s: &mut TcpStream, window: Duration) -> Option<(u16, Vec<u8>)> {
    tokio::time::timeout(window, async {
        let mut len = [0u8; 4];
        s.read_exact(&mut len).await.expect("frame length");
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        s.read_exact(&mut body).await.expect("frame body");
        (u16::from_le_bytes([body[0], body[1]]), body[2..].to_vec())
    })
    .await
    .ok()
}

/// Auth + join room 1; frames that race the join result are skipped.
async fn auth_and_join(s: &mut TcpStream, name: &str) {
    let auth = Auth {
        name: name.into(),
        ticket: Vec::new(),
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    write_frame(s, gsb_protocol::op::base::AUTH_REQ, &auth.encode_to_vec()).await;
    let join = JoinRoom { room_id: 1 };
    write_frame(
        s,
        gsb_protocol::op::base::JOIN_ROOM_REQ,
        &join.encode_to_vec(),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Some((op, payload)) = read_frame(s, Duration::from_millis(200)).await else {
            continue;
        };
        match op {
            gsb_protocol::op::base::AUTH_RESULT => {
                assert!(AuthResult::decode(&payload[..]).unwrap().ok, "auth");
            }
            gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                assert_ne!(JoinRoomResult::decode(&payload[..]).unwrap().entity, 0);
                return;
            }
            gsb_protocol::op::base::ERROR => {
                let e = Error::decode(&payload[..]).unwrap();
                panic!("{name}: join failed: code={} {}", e.code, e.message);
            }
            _ => {}
        }
    }
    panic!("{name}: timed out waiting for the join result");
}

/// Send one `ECONOMY` purchase and return the room's answer for it:
/// `(ok, reason, payload)` of the response carrying the request's id.
async fn buy_potion(s: &mut TcpStream, name: &str) -> (bool, String, Vec<u8>) {
    const ID: u64 = 7;
    let buy = gsb_demo::game::BuyItem {
        kind: "potion".into(),
    };
    let req = RpcRequest {
        id: ID,
        op: gsb_demo::op::ECONOMY as u32,
        payload: buy.encode_to_vec(),
    };
    write_frame(s, gsb_protocol::op::base::RPC_REQ, &req.encode_to_vec()).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Some((op, payload)) = read_frame(s, Duration::from_millis(200)).await else {
            continue;
        };
        if op != gsb_demo::op::PRIVATE {
            continue;
        }
        let private = gsb_demo::game::Private::decode(&payload[..]).unwrap();
        if let Some(r) = private.responses.iter().find(|r| r.id == ID) {
            assert_eq!(r.op as u16, gsb_demo::op::ECONOMY, "{name}: inner op");
            return (r.ok, r.reason.clone(), r.payload.clone());
        }
    }
    panic!("{name}: the ECONOMY request was never answered");
}

/// One ECONOMY round trip through the room build `cfg` selects.
async fn economy_answers(name: &str, cfg: Config) {
    let handle = gsb_server::start_server(cfg)
        .await
        .unwrap_or_else(|e| panic!("{name}: server starts: {e}"));
    let mut s = TcpStream::connect(handle.addr).await.expect("connect");
    auth_and_join(&mut s, name).await;
    let (ok, reason, payload) = buy_potion(&mut s, name).await;
    assert!(ok, "{name}: the ECONOMY request was rejected: {reason}");
    let result = gsb_demo::game::BuyResult::decode(&payload[..]).expect("BuyResult");
    assert!(
        result.ok,
        "{name}: the economy service must sell the potion"
    );
    assert_eq!(
        result.price, 100,
        "{name}: the price comes from the service"
    );
    handle.stop().await;
}

fn cfg(visibility: Visibility, topology: Option<Topology>) -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        visibility,
        topology,
        ..Default::default()
    }
}

#[tokio::test]
async fn economy_answers_in_the_open_room() {
    economy_answers("open", cfg(Visibility::All, None)).await;
}

#[tokio::test]
async fn economy_answers_in_the_aoi_room() {
    economy_answers("aoi", cfg(Visibility::Spatial, None)).await;
}

#[tokio::test]
async fn economy_answers_in_the_team_room() {
    economy_answers("team", cfg(Visibility::Team, None)).await;
}

#[tokio::test]
async fn economy_answers_in_the_sector_room() {
    economy_answers("sector", cfg(Visibility::Pvs, None)).await;
}

#[tokio::test]
async fn economy_answers_in_the_sharded_room() {
    economy_answers("sharded", cfg(Visibility::Sharded, None)).await;
}

#[tokio::test]
async fn economy_answers_in_the_sharded_spatial_room() {
    economy_answers(
        "sharded-spatial",
        cfg(Visibility::Spatial, Some(Topology::Sharded)),
    )
    .await;
}
