//! Byte compatibility across the kit/game boundary (KIT-ARCHITECTURE
//! §5): the kit's envelope (`gsb_kit::proto`, the opaque record run
//! `bytes records` and `bytes game`) and the war game's typed mirror
//! (`war.WorldSnapshot`, `war.Private` with `Welcome`) are two
//! definitions of ONE wire format — the war rides the kit's RECORD RUN
//! (A31): no `entities` entry, every unit `id varint + packed body` in
//! the one run. Pinned on constructed frames and on frames the real
//! shards emitted: a faction full, a delta with `removed`, a one-shot
//! private full carrying the welcome, an ack.

mod common;

use bytes::BytesMut;
use common::{War, realm};
use gsb_demo_war::codec::{WarWire, read_body, write_body};
use gsb_demo_war::components::Kind;
use gsb_demo_war::op;
use gsb_demo_war::war::{InputAck, Welcome, private as typed_private};
use gsb_kit::client::wire::{Fields, Value, varint};
use gsb_kit::proto as kit;
use gsb_protocol::base::RpcResponse;
use prost::Message;
use prost::encoding::encode_varint;

type Typed = gsb_demo_war::war::WorldSnapshot;
type TypedPrivate = gsb_demo_war::war::Private;

fn unit(x: i32, z: i32, kind: Kind, faction: u8, hp: u16) -> WarWire {
    WarWire {
        x,
        y: if kind == Kind::Tower { 120 } else { 0 },
        z,
        kind,
        faction,
        hp,
    }
}

/// A record run: `id varint + body` per unit.
fn run(units: &[(u64, WarWire)]) -> Vec<u8> {
    let mut out = BytesMut::new();
    for (id, w) in units {
        encode_varint(*id, &mut out);
        write_body(w, &mut out);
    }
    out.to_vec()
}

/// The kit form of a typed snapshot: the same run, opaque.
fn to_kit(t: &Typed) -> kit::WorldSnapshot {
    kit::WorldSnapshot {
        sequence: t.sequence,
        entities: Vec::new(),
        removed: t.removed.clone(),
        cell_exits: Vec::new(),
        delta: t.delta,
        records: t.records.clone(),
    }
}

/// Both definitions encode to the same bytes, and each decodes the
/// other's bytes back to itself.
fn same_wire<K: Message + Default + PartialEq, T: Message + Default + PartialEq>(
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
fn kit_envelope_and_war_mirror_encode_identically() {
    let full = Typed {
        sequence: 42,
        records: run(&[
            (1, unit(0, 0, Kind::Point, 0, 0)),
            (5, unit(-8_000, 8_000, Kind::Player, 1, 100)),
        ]),
        ..Default::default()
    };
    // The new mode, pinned: sequence, then ONE run field — the point
    // (id 1: head 6, x 0, z 0, hp 0), the player (id 5: head 2 | 1 << 3,
    // x zigzag 15 999, z zigzag 16 000, hp 100).
    assert_eq!(
        full.encode_to_vec(),
        [
            0x08, 0x2A, 0x32, 0x0C, 0x01, 0x06, 0x00, 0x00, 0x00, 0x05, 0x0A, 0xFF, 0x7C, 0x80,
            0x7D, 0x64
        ]
    );
    same_wire("full", &to_kit(&full), &full);
    let delta = Typed {
        sequence: 43,
        records: run(&[(129, unit(-64, 1, Kind::Tower, 3, 0))]),
        removed: vec![3, 7, 1 << 21],
        delta: true,
    };
    same_wire("delta", &to_kit(&delta), &delta);
    let removals_only = Typed {
        sequence: 44,
        removed: vec![9],
        delta: true,
        ..Default::default()
    };
    same_wire("removals only", &to_kit(&removals_only), &removals_only);
    let welcome = Welcome {
        faction: 2,
        factions: 3,
    };
    let responses = vec![RpcResponse {
        id: 7,
        ok: true,
        op: 1303,
        ..Default::default()
    }];
    let k = kit::Private {
        payload: Some(kit::private::Payload::Snapshot(to_kit(&full))),
        responses: responses.clone(),
        game: welcome.encode_to_vec(),
    };
    let t = TypedPrivate {
        payload: Some(typed_private::Payload::Snapshot(full)),
        responses,
        game: Some(welcome),
    };
    same_wire("one-shot full + welcome", &k, &t);
    let ack = Some(InputAck { processed_up_to: 9 });
    let k = kit::Private {
        payload: ack.map(kit::private::Payload::Ack),
        ..Default::default()
    };
    let t = TypedPrivate {
        payload: ack.map(typed_private::Payload::Ack),
        ..Default::default()
    };
    same_wire("ack", &k, &t);
}

/// One real snapshot through both definitions: same content, no
/// `entities` entry, one run whose every body re-encodes to its own
/// bytes; a FULL also re-encodes to the very bytes the kit wrote (a
/// delta cannot: the kit writes `removed` unpacked in the client's
/// apply order — the same message to every parser). Returns the units.
fn check(raw: &[u8], k: &kit::WorldSnapshot, t: &Typed) -> usize {
    assert_eq!(
        (k.sequence, k.delta, &k.removed, &k.records),
        (t.sequence, t.delta, &t.removed, &t.records)
    );
    assert!(k.entities.is_empty(), "the war's records ride the run");
    assert!(k.cell_exits.is_empty(), "team frames carry no cell exits");
    let runs = Fields::new(raw)
        .filter(|f| matches!(f, Ok((6, Value::Len(_)))))
        .count();
    let want = usize::from(!k.records.is_empty());
    assert_eq!(runs, want, "one run, and only when there are units");
    let (mut rest, mut units) = (&k.records[..], 0);
    while !rest.is_empty() {
        varint(&mut rest).expect("an id");
        let before = rest;
        let w = read_body(&mut rest).expect("a body");
        let mut again = BytesMut::new();
        write_body(&w, &mut again);
        assert_eq!(&again[..], &before[..before.len() - rest.len()]);
        units += 1;
    }
    assert_eq!(k.encode_to_vec(), t.encode_to_vec());
    if !k.delta {
        assert_eq!(
            k.encode_to_vec(),
            raw,
            "a full re-encodes to the kit's own bytes"
        );
    }
    units
}

#[tokio::test(start_paused = true)]
async fn real_shard_frames_decode_identically_through_both_definitions() {
    // Two faction-0 players on different shards (the far one's records
    // reach the other through the hub: imported bodies), an enemy by the
    // first; the far ally leaves (a `removed`); a late joiner gets the
    // one-shot full with its welcome; numbered moves are acked.
    let r = realm(&[
        ("a0", 0, -5.0, -300.0),
        ("far0", 0, 650.0, 150.0),
        ("e1", 1, 5.0, -300.0),
        ("late0", 0, -20.0, -300.0),
    ]);
    let mut war = War::new(&r).await;
    let a = war.join(1, "a0").await;
    let far = war.join(2, "far0").await;
    war.join(3, "e1").await;
    war.steps(35).await;
    war.clients[a].move_to(-6.0, -300.0);
    war.leave(far).await;
    war.steps(5).await;
    war.join(4, "late0").await;
    war.steps(5).await;

    let (mut fulls, mut removed, mut one_shots, mut welcomes, mut acks) = (0, 0, 0, 0, 0);
    let mut units = 0;
    for c in &war.clients {
        for (o, raw) in &c.raw {
            if *o == op::WAR_SNAPSHOT {
                let k = kit::WorldSnapshot::decode(&raw[..]).expect("kit decode");
                let t = Typed::decode(&raw[..]).expect("mirror decode");
                units += check(raw, &k, &t);
                fulls += usize::from(!t.delta);
                removed += usize::from(!t.removed.is_empty());
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
            if let Some(w) = &t.game {
                assert_eq!(k.game, w.encode_to_vec());
                welcomes += 1;
            }
            match (k.payload, t.payload) {
                (
                    Some(kit::private::Payload::Snapshot(ks)),
                    Some(typed_private::Payload::Snapshot(ts)),
                ) => {
                    assert!(!ts.delta, "a one-shot is a full");
                    units += check(&ks.encode_to_vec(), &ks, &ts);
                    one_shots += 1;
                }
                (Some(kit::private::Payload::Ack(x)), Some(typed_private::Payload::Ack(y))) => {
                    assert_eq!(x, y);
                    acks += 1;
                }
                (None, None) => {}
                other => panic!("the two definitions disagree: {other:?}"),
            }
        }
    }
    assert_eq!(welcomes, 4, "one per session");
    assert!(
        fulls >= 1 && one_shots >= 1 && acks >= 1,
        "{fulls} {one_shots} {acks}"
    );
    assert!(removed >= 1, "a real delta carried removed");
    assert!(units > 20, "the runs carried the units: {units}");
}
