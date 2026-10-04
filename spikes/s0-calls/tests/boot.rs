//! Versioning and the engine's own answers: opcodes collide at boot, an
//! unknown op is answered "no handler", an undecodable payload and a zero
//! id are answered by the engine. None of it reaches the game.

mod common;

use common::*;
use s0_calls::wire::{RpcRequest, reason};

#[derive(Clone, PartialEq, prost::Message)]
pub struct Impostor {
    #[prost(uint32, tag = "1")]
    pub x: u32,
}
impl Call for Impostor {
    type Response = Equipped;
    type Error = GameError;
}
impl Request for Impostor {
    const OP: u16 = Equip::OP; // reuses 1004
}

#[test]
fn an_opcode_collision_fails_at_boot() {
    let mut calls = Calls::new(CallsConfig::default());
    calls.accept::<Equip>().unwrap();
    match calls.accept::<Impostor>() {
        Err(BootError::OpCollision { op, first, second }) => {
            assert_eq!(op, 1004);
            assert!(first.ends_with("Equip") && second.ends_with("Impostor"));
        }
        other => panic!("{other:?}"),
    }
}

fn nothing(_: &mut (), calls: &mut Calls, _: &mut TickCtx) {
    assert!(
        calls.take_incoming_all().is_empty(),
        "the game sees none of it"
    );
}

#[test]
fn the_engine_answers_what_the_game_cannot_take() {
    let mut room = scripted(
        RoomConfig::default(),
        (),
        |c| c.accept::<Equip>().unwrap(),
        nothing,
    );
    let i = room.inputs();
    i.join(ALICE, "alice");
    i.request(
        ALICE,
        RpcRequest {
            id: 1,
            op: 4242,
            payload: vec![],
        },
    );
    i.request(
        ALICE,
        RpcRequest {
            id: 2,
            op: u32::from(Equip::OP),
            payload: vec![0xff, 0xff],
        },
    );
    i.request(
        ALICE,
        RpcRequest {
            id: 0,
            op: u32::from(Equip::OP),
            payload: vec![],
        },
    );
    let out = room.tick(0.0);
    let reasons: Vec<&str> = out.iter().map(|(_, r)| r.reason.as_str()).collect();
    assert_eq!(
        reasons,
        vec![reason::NO_HANDLER, reason::UNDECODABLE, reason::MALFORMED]
    );
    let c = room.counters();
    assert_eq!(
        (
            c.requests_no_handler,
            c.requests_undecodable,
            c.requests_malformed
        ),
        (1, 1, 1)
    );
}
