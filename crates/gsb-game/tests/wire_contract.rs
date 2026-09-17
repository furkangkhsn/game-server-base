//! Wire-contract locks for the frames whose DEFINITION moved between
//! .proto files without the bytes being allowed to move with it.
//!
//! The RPC response envelope was defined in `gsb-game/proto/game.proto`
//! (package `gsb.game`) and now lives in `gsb-protocol/proto/base.proto`
//! (package `gsb.base`) — see `docs/DESIGN.md` §5.2. Protobuf puts no
//! type names on the wire, so that move is a pure ownership refactor:
//! the field numbers and the encoded bytes are identical. These tests
//! are the proof, and they fail if a later edit changes a field number,
//! a field type, or the `Private.responses` tag.

use prost::Message;

use gsb_game::game::{InputAck, Private, WorldSnapshot, private};
use gsb_protocol::base::RpcResponse;

/// A `Private` frame carrying an input ack AND two RPC responses (an ok
/// one and a rejection) — the representative shape of the per-connection
/// per-tick frame.
///
/// The expected bytes were captured from the build in which `RpcResponse`
/// was still `gsb.game.RpcResponse` (commit 5f37c85's tree); they must
/// survive the move to `gsb.base`.
#[test]
fn private_with_ack_and_responses_is_byte_stable() {
    let frame = Private {
        payload: Some(private::Payload::Ack(InputAck {
            processed_up_to: 300,
        })),
        responses: vec![
            RpcResponse {
                id: 7,
                ok: true,
                op: 1005,
                reason: String::new(),
                payload: vec![0x08, 0x01],
            },
            RpcResponse {
                id: 8,
                ok: false,
                op: 1006,
                reason: "unknown item".into(),
                payload: Vec::new(),
            },
        ],
    };

    let expected: &[u8] = &[
        // field 1 (ack), LEN 3: { processed_up_to = 300 }
        0x0A, 0x03, 0x08, 0xAC, 0x02, //
        // field 3 (responses), LEN 11: { id=7, ok=true, op=1005, payload=[08 01] }
        0x1A, 0x0B, 0x08, 0x07, 0x10, 0x01, 0x18, 0xED, 0x07, 0x2A, 0x02, 0x08, 0x01,
        // field 3 (responses), LEN 19: { id=8, op=1006, reason="unknown item" }
        0x1A, 0x13, 0x08, 0x08, 0x18, 0xEE, 0x07, 0x22, 0x0C, b'u', b'n', b'k', b'n', b'o', b'w',
        b'n', b' ', b'i', b't', b'e', b'm',
    ];

    assert_eq!(
        frame.encode_to_vec(),
        expected,
        "the Private/RpcResponse wire encoding changed"
    );
}

/// The response envelope on its own: every field, every number.
#[test]
fn rpc_response_field_numbers_are_frozen() {
    let msg = RpcResponse {
        id: 1,
        ok: true,
        op: 2,
        reason: "r".into(),
        payload: vec![0xFF],
    };
    assert_eq!(
        msg.encode_to_vec(),
        &[
            0x08, 0x01, // 1: id      = 1   (varint)
            0x10, 0x01, // 2: ok      = true(varint)
            0x18, 0x02, // 3: op      = 2   (varint)
            0x22, 0x01, b'r', // 4: reason  = "r" (LEN)
            0x2A, 0x01, 0xFF, // 5: payload = FF  (LEN)
        ]
    );
}

/// The request envelope's numbers, for the same reason: request and
/// response are now one contract in one file and must stay pinned
/// together.
#[test]
fn rpc_request_field_numbers_are_frozen() {
    let msg = gsb_protocol::base::RpcRequest {
        id: 1,
        op: 2,
        payload: vec![0xFF],
    };
    assert_eq!(
        msg.encode_to_vec(),
        &[
            0x08, 0x01, // 1: id
            0x10, 0x02, // 2: op
            0x1A, 0x01, 0xFF, // 3: payload
        ]
    );
}

/// `Private.snapshot` (the oneof's second arm) keeps field number 2 and
/// its LEN tag `0x12` — the AOI one-shot path HAND-ENCODES that tag
/// (`aoi/logic.rs`), so a change here would silently desynchronise the
/// two encoders.
#[test]
fn private_snapshot_arm_keeps_its_hand_encoded_tag() {
    let frame = Private {
        payload: Some(private::Payload::Snapshot(WorldSnapshot {
            sequence: 1,
            ..Default::default()
        })),
        responses: Vec::new(),
    };
    assert_eq!(frame.encode_to_vec(), &[0x12, 0x02, 0x08, 0x01]);
}

/// A retired opcode must never be registered again. `gsb_game::op::
/// RETIRED` names the two the demo game removed in 2ac28d2
/// (ENTITY_SPAWNED = 1001, ENTITY_REMOVED = 1002); registering a new
/// message under one of them would silently misparse for any peer still
/// speaking the old protocol — the opcode-space equivalent of recycling
/// a protobuf field number, which `.proto` guards with `reserved` and
/// the opcode space has no keyword for.
#[test]
fn retired_opcodes_stay_out_of_the_message_table() {
    let mut table = gsb_protocol::base_table();
    gsb_game::register(&mut table);
    for op in gsb_game::op::RETIRED {
        assert!(
            !table.is_registered(op),
            "opcode {op} is retired (see gsb_game::op::RETIRED) but a \
             message is registered under it"
        );
    }
}
