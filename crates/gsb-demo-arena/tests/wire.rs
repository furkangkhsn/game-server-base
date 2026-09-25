//! Byte compatibility across the kit/arena boundary
//! (KIT-ARCHITECTURE §5): the kit's envelope (`gsb_kit::proto`, opaque
//! `repeated bytes` record bodies) and the arena's typed mirror
//! (`gsb_demo_arena::arena`, `repeated UnitRecord`) are two definitions
//! of ONE wire format. The kit writes the frames; an arena client decodes
//! them with the mirror. Pinned on constructed frames and on frames a
//! real arena room emitted through the room actor.

mod common;

use common::{Arena, SETTLE};
use gsb_demo_arena::ArenaGame;
use gsb_demo_arena::arena::{InputAck, UnitRecord, Welcome};
use gsb_kit::proto as kit;
use gsb_protocol::base::RpcResponse;
use prost::Message;

type Typed = gsb_demo_arena::arena::WorldSnapshot;
type TypedPrivate = gsb_demo_arena::arena::Private;
use gsb_demo_arena::arena::private as typed_private;

fn rec(entity: u64, x: i32, y: i32, z: i32) -> UnitRecord {
    UnitRecord { entity, x, y, z }
}

/// The kit form of a typed snapshot: every record as its opaque body.
fn to_kit(t: &Typed) -> kit::WorldSnapshot {
    kit::WorldSnapshot {
        sequence: t.sequence,
        entities: t.entities.iter().map(Message::encode_to_vec).collect(),
        removed: t.removed.clone(),
        cell_exits: Vec::new(),
        delta: t.delta,
    }
}

/// Both definitions encode to the same bytes, and each decodes the
/// other's bytes back to itself.
fn assert_same_wire<K: Message + Default + PartialEq, T: Message + Default + PartialEq>(
    name: &str,
    kit_msg: &K,
    typed_msg: &T,
) {
    let (kb, tb) = (kit_msg.encode_to_vec(), typed_msg.encode_to_vec());
    assert_eq!(kb, tb, "{name}: identical bytes");
    assert!(
        K::decode(&tb[..]).expect("kit decodes") == *kit_msg,
        "{name}"
    );
    assert!(
        T::decode(&kb[..]).expect("mirror decodes") == *typed_msg,
        "{name}"
    );
}

#[test]
fn kit_envelope_and_arena_mirror_encode_identically() {
    let full = Typed {
        sequence: 42,
        entities: vec![
            rec(1, 0, 0, 0),
            rec(2, -5000, 3000, 5000),
            rec(300, 123, 568, -1000),
        ],
        ..Default::default()
    };
    assert_same_wire("full", &to_kit(&full), &full);
    let delta = Typed {
        sequence: 43,
        entities: vec![rec(129, -64, 64, 1)],
        removed: vec![3, 7, 128],
        delta: true,
    };
    assert_same_wire("delta", &to_kit(&delta), &delta);

    let responses = vec![RpcResponse {
        id: 7,
        ok: false,
        op: 1100,
        reason: "no".into(),
        payload: vec![1],
    }];
    let ack = Some(InputAck {
        processed_up_to: 300,
    });
    let kit_ack = kit::Private {
        payload: ack.map(kit::private::Payload::Ack),
        responses: responses.clone(),
        game: Vec::new(),
    };
    let typed_ack = TypedPrivate {
        payload: ack.map(typed_private::Payload::Ack),
        responses,
        game: None,
    };
    assert_same_wire("private ack", &kit_ack, &typed_ack);
    let kit_snap = kit::Private {
        payload: Some(kit::private::Payload::Snapshot(to_kit(&full))),
        ..Default::default()
    };
    let typed_snap = TypedPrivate {
        payload: Some(typed_private::Payload::Snapshot(full)),
        ..Default::default()
    };
    assert_same_wire("private snapshot", &kit_snap, &typed_snap);
}

/// Real frames: a three-team room's team snapshot and an ack-carrying
/// private frame, exactly as the room actor delivered them, decode
/// through both definitions to the same content (every kit record body
/// IS the typed `UnitRecord` encoding) and both re-encode to the very
/// bytes the kit wrote by hand.
#[tokio::test]
async fn real_room_frames_decode_identically_through_both_definitions() {
    let mut arena = Arena::new(ArenaGame::default());
    let mut cs = vec![
        arena.join(1).await,
        arena.join(2).await,
        arena.join(3).await,
    ];
    cs[0].move_to(1.23, 5.68, -10.0, 1).await;
    cs[1].move_to(4.0, 0.0, -10.0, 0).await; // in A's vision
    cs[2].move_to(-40.0, 30.0, 40.0, 0).await; // out of everyone's
    arena.advance(&mut cs, SETTLE).await;

    let raw = cs[0].last_snapshot.clone().expect("a team snapshot");
    let k = kit::WorldSnapshot::decode(&raw[..]).expect("kit decode");
    let t = Typed::decode(&raw[..]).expect("typed decode");
    assert_eq!((k.sequence, k.delta), (t.sequence, t.delta));
    assert!(k.removed.is_empty() && k.cell_exits.is_empty() && !t.delta);
    let bodies: Vec<Vec<u8>> = t.entities.iter().map(Message::encode_to_vec).collect();
    assert_eq!(k.entities, bodies, "record bodies");
    assert_eq!(k.encode_to_vec(), &raw[..], "the kit definition re-encodes");
    assert_eq!(t.encode_to_vec(), &raw[..], "the mirror re-encodes");
    let mut got: Vec<UnitRecord> = t.entities.clone();
    got.sort_by_key(|r| r.entity);
    assert_eq!(
        got,
        [rec(cs[0].id, 123, 568, -1000), rec(cs[1].id, 400, 0, -1000)],
        "team 0's view, in centimetres"
    );

    let raw = cs[0].last_private.clone().expect("the ack frame");
    let k = kit::Private::decode(&raw[..]).expect("kit decode");
    let t = TypedPrivate::decode(&raw[..]).expect("typed decode");
    assert_eq!(k.encode_to_vec(), &raw[..]);
    assert_eq!(t.encode_to_vec(), &raw[..]);
    assert!(k.game.is_empty(), "the welcome rode the first frame only");
    let ack = Some(InputAck { processed_up_to: 1 });
    assert_eq!(k.payload, ack.map(kit::private::Payload::Ack));
    assert_eq!(t.payload, ack.map(typed_private::Payload::Ack));
}

/// The arena's deliberate wire change (GAME-MODULE G3-3): each joiner's
/// first private frame is exactly its `Welcome` in the kit's field 4 —
/// no ack yet, nothing else — pinned byte for byte; the kit's opaque
/// `bytes game` and the mirror's typed `Welcome game` read the same
/// bytes. Team 0 omits its default `team` (the field is still there).
/// Nothing else changes: the next frames carry no welcome.
#[tokio::test]
async fn each_joiner_is_welcomed_with_its_team_once() {
    let mut arena = Arena::new(ArenaGame::default());
    let mut cs = vec![
        arena.join(1).await,
        arena.join(2).await,
        arena.join(3).await,
        arena.join(4).await,
    ];
    cs[0].move_to(1.0, 0.0, 0.0, 1).await;
    arena.advance(&mut cs, 3).await;

    let pinned: [&[u8]; 4] = [
        &[0x22, 0x02, 0x10, 0x03],
        &[0x22, 0x04, 0x08, 0x01, 0x10, 0x03],
        &[0x22, 0x04, 0x08, 0x02, 0x10, 0x03],
        &[0x22, 0x02, 0x10, 0x03],
    ];
    for (i, (c, bytes)) in cs.iter().zip(pinned).enumerate() {
        let raw = c.first_private.clone().expect("a first private frame");
        assert_eq!(&raw[..], bytes, "joiner {i}: the welcome, byte for byte");
        let team = (i % 3) as u32;
        let welcome = Welcome { team, teams: 3 };
        let k = kit::Private::decode(&raw[..]).expect("kit decode");
        assert_eq!(k.game, welcome.encode_to_vec(), "joiner {i}: kit view");
        let t = TypedPrivate::decode(&raw[..]).expect("typed decode");
        assert_eq!(t.game, Some(welcome), "joiner {i}: typed view");
        assert_eq!(t.encode_to_vec(), &raw[..], "joiner {i}: re-encodes");
        assert_eq!(c.welcomes, [welcome], "joiner {i}: once");
    }
    assert_eq!(cs[0].acks, [1], "the ack comes in its own later frame");
}
