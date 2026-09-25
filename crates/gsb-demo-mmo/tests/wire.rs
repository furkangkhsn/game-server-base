//! Byte compatibility across the kit/MMO boundary (KIT-ARCHITECTURE §5):
//! the kit's envelope (`gsb_kit::proto`, opaque `repeated bytes` record
//! and cell-exit bodies) and the MMO's typed mirror (`mmo.WorldSnapshot`
//! with `EntityRecord` / ground-plane `CellExit`) are two definitions of
//! ONE wire format. Pinned on constructed frames and on frames a real
//! MMO shard emitted through the shard actor: a group full, deltas with
//! `removed` and with `cell_exits`, a one-shot private full, an ack.

mod common;

use common::Mmo;
use gsb_demo_mmo::mmo::{CellExit, EntityRecord, InputAck};
use gsb_demo_mmo::{MobSpawn, Pos3, Realm, components::Kind, op};
use gsb_kit::proto as kit;
use gsb_protocol::base::RpcResponse;
use prost::Message;

type Typed = gsb_demo_mmo::mmo::WorldSnapshot;
type TypedPrivate = gsb_demo_mmo::mmo::Private;
use gsb_demo_mmo::mmo::private as typed_private;

fn rec(entity: u64, x: i32, y: i32, z: i32, hp: u32) -> EntityRecord {
    let kind = gsb_demo_mmo::mmo::Kind::Mob as i32;
    EntityRecord {
        entity,
        x,
        y,
        z,
        kind,
        hp,
    }
}

/// The kit form of a typed snapshot: every record / cell exit as the
/// opaque body the MMO's codec / the kit's `Grid2` writes.
fn to_kit(t: &Typed) -> kit::WorldSnapshot {
    kit::WorldSnapshot {
        sequence: t.sequence,
        entities: t.entities.iter().map(Message::encode_to_vec).collect(),
        removed: t.removed.clone(),
        cell_exits: t.cell_exits.iter().map(Message::encode_to_vec).collect(),
        delta: t.delta,
        // The record run (A31): absent — this game did not opt in.
        records: Vec::new(),
    }
}

/// Both definitions encode to the same bytes, and each decodes the
/// other's bytes back to itself.
fn assert_same_wire<K: Message + Default + PartialEq, T: Message + Default + PartialEq>(
    name: &str,
    k: &K,
    t: &T,
) {
    let (kb, tb) = (k.encode_to_vec(), t.encode_to_vec());
    assert_eq!(kb, tb, "{name}: identical bytes");
    assert!(K::decode(&tb[..]).expect("kit decodes") == *k, "{name}");
    assert!(T::decode(&kb[..]).expect("mirror decodes") == *t, "{name}");
}

#[test]
fn kit_envelope_and_mmo_mirror_encode_identically() {
    let full = Typed {
        sequence: 42,
        entities: vec![rec(1, 0, 0, 0, 0), rec(3 << 20, -5120, 2000, 5120, 300)],
        ..Default::default()
    };
    assert_same_wire("full", &to_kit(&full), &full);
    let delta = Typed {
        sequence: 43,
        entities: vec![rec(129, -64, 150, 1, 25)],
        removed: vec![3, 7, 1 << 20],
        cell_exits: vec![CellExit { x: 0, z: -1 }, CellExit { x: 7, z: 0 }],
        delta: true,
    };
    assert_same_wire("delta", &to_kit(&delta), &delta);
    let ack = Some(InputAck { processed_up_to: 9 });
    let responses = vec![RpcResponse {
        id: 7,
        ok: true,
        op: 1203,
        ..Default::default()
    }];
    let k = kit::Private {
        payload: ack.map(kit::private::Payload::Ack),
        responses: responses.clone(),
        game: Vec::new(),
    };
    let t = TypedPrivate {
        payload: ack.map(typed_private::Payload::Ack),
        responses,
    };
    assert_same_wire("private ack", &k, &t);
    let k = kit::Private {
        payload: Some(kit::private::Payload::Snapshot(to_kit(&full))),
        ..Default::default()
    };
    let t = TypedPrivate {
        payload: Some(typed_private::Payload::Snapshot(full)),
        ..Default::default()
    };
    assert_same_wire("private full", &k, &t);
}

/// One real snapshot through both definitions: same content (every kit
/// body IS the typed encoding) and identical re-encodings. A FULL also
/// re-encodes to the very bytes the kit wrote by hand; a delta cannot:
/// the kit writes its fields in the client's apply order (`delta` in the
/// header, then `removed` unpacked, `cell_exits`, `entities`), a
/// generated encoder in field-number order with `removed` packed — the
/// same message to every protobuf parser.
fn check(raw: &[u8], k: &kit::WorldSnapshot, t: &Typed) {
    assert_eq!(
        (k.sequence, k.delta, &k.removed),
        (t.sequence, t.delta, &t.removed)
    );
    assert_eq!(
        k.entities,
        t.entities
            .iter()
            .map(Message::encode_to_vec)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        k.cell_exits,
        t.cell_exits
            .iter()
            .map(Message::encode_to_vec)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        k.encode_to_vec(),
        t.encode_to_vec(),
        "the definitions re-encode identically"
    );
    if !k.delta {
        assert_eq!(
            k.encode_to_vec(),
            raw,
            "a full re-encodes to the kit's own bytes"
        );
    }
}

#[tokio::test]
async fn real_shard_frames_decode_identically_through_both_definitions() {
    // O in cell (3,3). M1 walks out of cell (4,3), which M2 keeps
    // populated (a `removed`); M3 walks out of cell (2,4), which empties
    // (a `cell_exits` entry). Q joins O's established group later (a
    // one-shot private full).
    let walker = |x: f32, z: f32, to: [f32; 2]| {
        MobSpawn::once(Kind::Mob, Pos3::new(x, 0.0, z), 1, 100_000, 80).walking(
            vec![to],
            3.0,
            false,
        )
    };
    let realm = Realm::empty()
        .with_login("c1", Pos3::new(224.0, 0.0, 224.0))
        .with_login("c2", Pos3::new(230.0, 0.0, 200.0))
        .with_spawn(walker(315.0, 230.0, [330.0, 230.0]))
        .with_spawn(MobSpawn::once(
            Kind::Mob,
            Pos3::new(300.0, 0.0, 250.0),
            1,
            100_000,
            80,
        ))
        .with_spawn(walker(150.0, 315.0, [150.0, 330.0]));
    let mut room = Mmo::new(&realm);
    let mut cs = vec![room.join(1, "c1", &mut []).await];
    room.steps(&mut cs, 5).await;
    cs[0].move_to(226.0, 224.0).await;
    let q = room.join(2, "c2", &mut cs).await;
    cs.push(q);
    room.steps(&mut cs, 80).await;

    let (mut fulls, mut removed, mut exits, mut one_shots, mut acks) = (0, 0, 0, 0, 0);
    for c in &cs {
        for (o, raw) in &c.raw {
            if *o == op::MMO_SNAPSHOT {
                let k = kit::WorldSnapshot::decode(&raw[..]).expect("kit decode");
                let t = Typed::decode(&raw[..]).expect("mirror decode");
                check(raw, &k, &t);
                fulls += usize::from(!t.delta);
                removed += usize::from(!t.removed.is_empty());
                exits += usize::from(t.cell_exits.contains(&CellExit { x: 2, z: 4 }));
                continue;
            }
            let k = kit::Private::decode(&raw[..]).expect("kit decode");
            let t = TypedPrivate::decode(&raw[..]).expect("mirror decode");
            let kb = k.encode_to_vec();
            assert_eq!(
                kb,
                t.encode_to_vec(),
                "the definitions re-encode identically"
            );
            assert_eq!(kb, &raw[..], "and to the kit's own bytes");
            assert!(k.game.is_empty(), "the MMO ships no private game payload");
            match (k.payload, t.payload) {
                (
                    Some(kit::private::Payload::Snapshot(ks)),
                    Some(typed_private::Payload::Snapshot(ts)),
                ) => {
                    assert!(!ts.delta, "a one-shot is a full");
                    check(&ks.encode_to_vec(), &ks, &ts);
                    one_shots += 1;
                }
                (Some(kit::private::Payload::Ack(a)), Some(typed_private::Payload::Ack(b))) => {
                    assert_eq!(a, b);
                    acks += 1;
                }
                other => panic!("the two definitions disagree: {other:?}"),
            }
        }
    }
    assert!(
        fulls >= 1 && one_shots >= 1 && acks >= 1,
        "{fulls} {one_shots} {acks}"
    );
    assert!(
        removed >= 1 && exits >= 1,
        "real deltas carried removed ({removed}) and cell exits ({exits})"
    );
}
