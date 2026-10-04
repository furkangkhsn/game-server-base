//! The typed request table: duplicate opcodes are refused at
//! registration; unknown opcodes and undecodable payloads are answered
//! and counted by the engine and never reach the game.

mod common;

use common::*;
use prost::Message;
use s0_calls::fake::FakeService;
use s0_calls::request::{ConnId, DuplicateOp, RequestTable};
use s0_calls::room::{Action, Inbound, Room, RoomConfig};
use s0_calls::wire::{self, reason};

#[test]
fn a_duplicate_opcode_is_refused_at_registration() {
    let mut t: RequestTable<ShopReq> = RequestTable::default();
    t.add::<BuyItem>(ShopReq::Buy).unwrap();
    assert_eq!(t.add::<BuyItem>(ShopReq::Buy), Err(DuplicateOp(1001)));
}

#[test]
fn bad_requests_are_the_engines_business() {
    let (econ, _fake) = FakeService::new("economy", 8);
    let mut room = Room::new(RoomConfig::default(), Shop::new(econ, 0), shop_table());
    let c = ConnId(1);
    let (tx, mut out) = room.join(c, 16);
    let env = |id: u64, op: u32, payload: Vec<u8>| {
        Inbound::Action(Action {
            conn: c,
            op: wire::op::RPC_REQ,
            payload: wire::RpcRequest {
                id,
                op,
                payload,
                target: 0,
            }
            .encode_to_vec(),
        })
    };
    tx.try_send(env(1, 4242, Vec::new())).unwrap(); // no handler
    tx.try_send(env(2, 1001, vec![0xff, 0xff, 0xff])).unwrap(); // undecodable
    tx.try_send(env(0, 1001, Vec::new())).unwrap(); // id 0
    room.tick();

    assert!(room.logic().trace_at(1).iter().all(|s| *s != "request"));
    assert_eq!(room.counters.requests_rejected_no_handler, 1);
    assert_eq!(room.counters.requests_rejected_undecodable, 1);
    assert_eq!(room.counters.requests_rejected_malformed, 1);
    let got = responses(&mut out);
    let reasons: Vec<_> = got.iter().map(|r| r.reason.as_str()).collect();
    assert!(reasons.contains(&reason::NO_HANDLER));
    assert!(reasons.contains(&reason::UNDECODABLE));
    assert!(reasons.contains(&reason::MALFORMED));
    assert!(
        got.iter()
            .all(|r| !r.ok && !r.game_error && r.payload.is_empty()),
        "engine shape"
    );
}
