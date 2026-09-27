//! `with_economy` on every single-world demo room (BACKLOG F4): a room
//! built with it delegates an `ECONOMY` request to THAT service (the
//! answer carries the service's price), one built without it rejects
//! the request as "not configured". The end-to-end twin is
//! `gsb-server`'s `tests/economy_rooms.rs`.

use std::time::Duration;

use gsb_core::id::PlayerId;
use gsb_core::rpc::{RequestDecision, RpcRequest};

use super::*;
use crate::aoi::AoiRoom;
use crate::demo::economy::EconomyService;
use crate::demo::game::{BuyItem, BuyResult};
use crate::demo::op;
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::team::TeamRoom;

/// What the room made of one `ECONOMY` purchase of a potion: the price
/// the service charged, or the room's rejection.
async fn buy_potion<R: GameLogic<World>>(mut room: R) -> Result<u32, String> {
    let req = RpcRequest {
        conn: ConnectionId(1),
        player: PlayerId(1),
        id: 1,
        op: op::ECONOMY,
        payload: BuyItem {
            kind: "potion".into(),
        }
        .encode_to_vec()
        .into(),
    };
    match room.handle_request(&mut World::new(), &ctx1(), &req) {
        Some(RequestDecision::External(answer)) => {
            let body = answer.await?;
            Ok(BuyResult::decode(body.as_ref()).expect("BuyResult").price)
        }
        Some(RequestDecision::Reject(reason)) => Err(reason),
        Some(RequestDecision::Reply(_)) => panic!("an ECONOMY request answered in-room"),
        None => panic!("no handler for ECONOMY"),
    }
}

fn economy() -> EconomyService {
    EconomyService::spawn(Duration::ZERO)
}

const POTION: Result<u32, String> = Ok(100);

#[tokio::test]
async fn the_open_room_delegates_to_its_economy() {
    assert_eq!(
        buy_potion(OpenRoom::new().with_economy(economy())).await,
        POTION
    );
}

#[tokio::test]
async fn the_aoi_room_delegates_to_its_economy() {
    let room = AoiRoom::new(20.0).with_economy(economy());
    assert_eq!(buy_potion(room).await, POTION);
}

#[tokio::test]
async fn the_team_room_delegates_to_its_economy() {
    let room = TeamRoom::new(30.0).with_economy(economy());
    assert_eq!(buy_potion(room).await, POTION);
}

#[tokio::test]
async fn the_sector_room_delegates_to_its_economy() {
    let room = SectorRoom::new().with_economy(economy());
    assert_eq!(buy_potion(room).await, POTION);
}

/// The control: without `with_economy` the same request is a normal
/// rejection, so the tests above see the builder, not a default.
#[tokio::test]
async fn a_room_without_an_economy_rejects_the_purchase() {
    let not_configured = Err("economy service not configured".to_string());
    assert_eq!(buy_potion(OpenRoom::new()).await, not_configured);
    assert_eq!(buy_potion(AoiRoom::new(20.0)).await, not_configured);
    assert_eq!(buy_potion(TeamRoom::new(30.0)).await, not_configured);
    assert_eq!(buy_potion(SectorRoom::new()).await, not_configured);
}
