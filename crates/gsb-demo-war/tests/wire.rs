//! Byte compatibility across the kit/game boundary (KIT-ARCHITECTURE
//! §5): the kit's envelope (`gsb_kit::proto`, opaque `repeated bytes`
//! record bodies and `bytes game`) and the war game's typed mirror
//! (`war.WorldSnapshot` with `UnitRecord`, `war.Private` with `Welcome`)
//! are two definitions of ONE wire format. Pinned on constructed frames
//! and on frames the real shards emitted: a faction full, a delta with
//! `removed`, a one-shot private full carrying the welcome, an ack.

mod common;

use common::{War, realm};
use gsb_demo_war::op;
use gsb_demo_war::war::{InputAck, Kind, UnitRecord, Welcome, private as typed_private};
use gsb_kit::proto as kit;
use gsb_protocol::base::RpcResponse;
use prost::Message;

type Typed = gsb_demo_war::war::WorldSnapshot;
type TypedPrivate = gsb_demo_war::war::Private;

fn rec(entity: u64, x: i32, z: i32, faction: u32) -> UnitRecord {
    UnitRecord {
        entity,
        x,
        y: 120,
        z,
        kind: Kind::Tower as i32,
        faction,
        hp: 0,
    }
}

/// The kit form of a typed snapshot: every record as the opaque body the
/// game's codec writes.
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
        entities: vec![rec(1, 0, 0, 0), rec(3 << 21, -8_000, 8_000, 3)],
        ..Default::default()
    };
    same_wire("full", &to_kit(&full), &full);
    let delta = Typed {
        sequence: 43,
        entities: vec![rec(129, -64, 1, 1)],
        removed: vec![3, 7, 1 << 21],
        delta: true,
    };
    same_wire("delta", &to_kit(&delta), &delta);
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

/// One real snapshot through both definitions: same content and
/// identical re-encodings; a FULL also re-encodes to the very bytes the
/// kit wrote (a delta cannot: the kit writes `removed` unpacked in the
/// client's apply order — the same message to every parser).
fn check(raw: &[u8], k: &kit::WorldSnapshot, t: &Typed) {
    assert_eq!(
        (k.sequence, k.delta, &k.removed),
        (t.sequence, t.delta, &t.removed)
    );
    let typed: Vec<Vec<u8>> = t.entities.iter().map(Message::encode_to_vec).collect();
    assert_eq!(k.entities, typed);
    assert!(k.cell_exits.is_empty(), "team frames carry no cell exits");
    assert_eq!(k.encode_to_vec(), t.encode_to_vec());
    if !k.delta {
        assert_eq!(
            k.encode_to_vec(),
            raw,
            "a full re-encodes to the kit's own bytes"
        );
    }
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
    for c in &war.clients {
        for (o, raw) in &c.raw {
            if *o == op::WAR_SNAPSHOT {
                let k = kit::WorldSnapshot::decode(&raw[..]).expect("kit decode");
                let t = Typed::decode(&raw[..]).expect("mirror decode");
                check(raw, &k, &t);
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
                    check(&ks.encode_to_vec(), &ks, &ts);
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
}
