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

use gsb_client::session::{self, Credentials};
use gsb_client::{Conn, Recv};
use gsb_protocol::base::RpcRequest;
use gsb_server::{Config, Topology, Visibility};
use prost::Message;

/// Auth + join room 1; frames that race the join result are skipped.
async fn auth_and_join(s: &mut Conn, name: &str) {
    let joined = session::auth_and_join(
        s,
        &Credentials::named(name),
        1,
        Duration::from_secs(10),
        |_| {},
    )
    .await
    .unwrap_or_else(|e| panic!("{name}: join failed: {e}"));
    assert_ne!(joined.entity, 0);
}

/// Send one `ECONOMY` purchase and return the room's answer for it:
/// `(ok, reason, payload)` of the response carrying the request's id.
async fn buy_potion(s: &mut Conn, name: &str) -> (bool, String, Vec<u8>) {
    const ID: u64 = 7;
    let buy = gsb_demo::game::BuyItem {
        kind: "potion".into(),
    };
    let req = RpcRequest {
        id: ID,
        op: gsb_demo::op::ECONOMY as u32,
        payload: buy.encode_to_vec(),
    };
    s.send(gsb_protocol::op::base::RPC_REQ, &req.encode_to_vec())
        .await
        .expect("write frame");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Recv::Frame(f) = s.recv(Duration::from_millis(200)).await.expect("frame") else {
            continue;
        };
        let (op, payload) = (f.op, f.payload);
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
    let mut s = gsb_client::connect::tcp(handle.addr)
        .await
        .expect("connect");
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
