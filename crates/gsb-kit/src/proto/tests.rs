//! The kit envelope's field numbers and wire types, frozen: the kit
//! hand-encodes these frames (tags `0x08`, `0x12`, `0x18`, `0x22`,
//! `0x28` in the snapshot; `0x12` and `0x1A` in the private frame), and
//! every game's typed mirror must keep the same numbers. A renumbering in
//! `kit.proto` breaks these before it can desynchronise the encoders.

use prost::Message;

use super::*;

/// Every `WorldSnapshot` field, in field order.
#[test]
fn world_snapshot_field_numbers_are_frozen() {
    let snap = WorldSnapshot {
        sequence: 300,
        entities: vec![vec![0x08, 0x01], vec![0x08, 0x02, 0x10, 0x02]],
        removed: vec![5, 200],
        cell_exits: vec![vec![0x08, 0x02]],
        delta: true,
    };
    let expected: &[u8] = &[
        0x08, 0xAC, 0x02, // 1: sequence = 300
        0x12, 0x02, 0x08, 0x01, // 2: entities[0] (an opaque record body)
        0x12, 0x04, 0x08, 0x02, 0x10, 0x02, // 2: entities[1]
        0x1A, 0x03, 0x05, 0xC8, 0x01, // 3: removed = [5, 200] (packed)
        0x22, 0x02, 0x08, 0x02, // 4: cell_exits[0] (an opaque cell body)
        0x28, 0x01, // 5: delta = true
    ];
    assert_eq!(snap.encode_to_vec(), expected);
}

/// Every `Private` field: the ack arm, the snapshot arm (the tag the AOI
/// and sharded × spatial one-shot paths hand-encode), the responses (the
/// other hand-encoded tag) and the game's own payload slot.
#[test]
fn private_field_numbers_are_frozen() {
    let ack = Private {
        payload: Some(private::Payload::Ack(InputAck {
            processed_up_to: 300,
        })),
        ..Default::default()
    };
    assert_eq!(ack.encode_to_vec(), &[0x0A, 0x03, 0x08, 0xAC, 0x02]);

    let full = Private {
        payload: Some(private::Payload::Snapshot(WorldSnapshot {
            sequence: 1,
            ..Default::default()
        })),
        responses: vec![gsb_protocol::base::RpcResponse {
            id: 7,
            ok: true,
            op: 1005,
            reason: String::new(),
            payload: vec![0x08, 0x01],
        }],
        game: vec![0xAB],
    };
    let expected: &[u8] = &[
        0x12, 0x02, 0x08, 0x01, // 2: snapshot { sequence = 1 }
        // 3: responses[0] { id=7, ok=true, op=1005, payload=[08 01] }
        0x1A, 0x0B, 0x08, 0x07, 0x10, 0x01, 0x18, 0xED, 0x07, 0x2A, 0x02, 0x08, 0x01, 0x22, 0x01,
        0xAB, // 4: game = [AB]
    ];
    assert_eq!(full.encode_to_vec(), expected);
}
