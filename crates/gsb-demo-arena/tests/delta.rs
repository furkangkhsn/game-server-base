//! The arena's delta mode against its full mode, through the real room
//! actor: two rooms — the arena's own (delta, 15 Hz records) and the
//! team room's default (full only — full frames ignore the send rate)
//! — play the same inputs; at every checkpoint, while units are still
//! moving through each other's 3D fog, every client of the delta room
//! holds the same units its twin in the full room holds, each where the
//! full room showed it this tick or the one before (the arena's send
//! rate: a unit's move goes out on every 2nd step, KIT-ARCHITECTURE §10
//! "A10"). The delta room's clients apply deltas (a unit entering,
//! moving in and leaving a team's view); the full room's never see one.
//! And a real delta frame, byte for byte against both definitions.

mod common;

use common::{Arena, Client, SETTLE};
use gsb_demo_arena::ArenaGame;
use gsb_kit::proto as kit;
use prost::Message;

type Typed = gsb_demo_arena::arena::WorldSnapshot;

/// Six units (two per team), then waves of moves that cross the teams'
/// vision on the floor and in height; `(client, x, y, z)` per move.
const WAVES: [&[(usize, f32, f32, f32)]; 4] = [
    &[
        (0, 0.0, 0.0, 0.0),
        (1, 10.0, 0.0, 0.0),
        (2, -10.0, 0.0, 5.0),
        (3, -30.0, 0.0, -30.0),
        (4, 30.0, 0.0, 30.0),
        (5, 0.0, 20.0, 0.0),
    ],
    &[(1, 40.0, 0.0, 0.0), (5, 0.0, 2.0, 0.0), (3, 0.0, 0.0, -8.0)],
    &[
        (0, -30.0, 0.0, -30.0),
        (2, 30.0, 0.0, 30.0),
        (4, 0.0, 25.0, 0.0),
    ],
    &[(1, 0.0, 0.0, 0.0), (3, 0.0, 0.0, 0.0), (5, 0.0, 0.0, 0.0)],
];

async fn joined(arena: &mut Arena) -> Vec<Client> {
    let mut cs = Vec::new();
    for conn in 1..=6 {
        cs.push(arena.join(conn).await);
    }
    cs
}

#[tokio::test]
async fn delta_and_full_rooms_agree_within_the_send_rate() {
    let mut delta = Arena::new(ArenaGame::default());
    let mut full = Arena::full_only(ArenaGame::default());
    let mut ds = joined(&mut delta).await;
    let mut fs = joined(&mut full).await;
    let (mut checkpoints, mut behind) = (0, 0);
    for wave in WAVES {
        for &(i, x, y, z) in wave {
            ds[i].move_to(x, y, z, 0).await;
            fs[i].move_to(x, y, z, 0).await;
        }
        // Checkpoints every 7 ticks for ~4 s: the units are mid-flight
        // (12 m/s) for most of them. The full room's views one tick
        // before the checkpoint are the delta room's staleness bound.
        for _ in 0..16 {
            delta.advance(&mut ds, 6).await;
            full.advance(&mut fs, 6).await;
            full.flush(&mut fs).await;
            let before: Vec<_> = fs.iter().map(|f| f.view.clone()).collect();
            delta.advance(&mut ds, 1).await;
            full.advance(&mut fs, 1).await;
            delta.flush(&mut ds).await;
            full.flush(&mut fs).await;
            for (i, (d, f)) in ds.iter().zip(&fs).enumerate() {
                assert_eq!(d.id, f.id, "the twins mint alike");
                assert!(
                    d.view.keys().eq(f.view.keys()),
                    "checkpoint {checkpoints}: client {i} sees the same units"
                );
                for (id, at) in &d.view {
                    let now = f.view[id] == *at;
                    assert!(
                        now || before[i].get(id) == Some(at),
                        "checkpoint {checkpoints}: client {i}, unit {id} at {at:?}"
                    );
                    behind += u32::from(!now);
                }
            }
            checkpoints += 1;
        }
    }
    let deltas: u32 = ds.iter().map(|c| c.deltas).sum();
    assert!(deltas > 100, "the delta room shipped deltas: {deltas}");
    assert!(behind > 20, "moves waited for their step: {behind}");
    assert!(fs.iter().all(|c| c.deltas == 0 && c.gap_drops == 0));
    for (i, c) in ds.iter().enumerate() {
        // Joins 4..6 found their team playing: one delta dropped before
        // the one-shot full; the first three were baselined by their
        // fresh team's full.
        let late = u32::from(i >= 3);
        assert_eq!((c.gap_drops, c.private_fulls), (late, late), "client {i}");
    }
}

/// A real DELTA frame (the arena's room): a unit leaving a team's
/// vision while another moves in it — `removed` and upserts in one frame
/// — decodes through both definitions to the same content, and its
/// bytes are the kit's layout: the header (`sequence`, `delta`), one
/// UNPACKED `removed` entry per id, then the records (a generated
/// encoder packs `removed`; both parse alike, `kit.proto`).
#[tokio::test]
async fn real_delta_frames_decode_identically_through_both_definitions() {
    let mut arena = Arena::new(ArenaGame::default());
    let mut cs = vec![arena.join(1).await, arena.join(2).await];
    cs[0].move_to(0.0, 0.0, 0.0, 0).await;
    cs[1].move_to(8.0, 0.0, 0.0, 0).await; // in A's vision
    arena.advance(&mut cs, SETTLE).await;
    assert!(cs[0].sees().contains(&cs[1].id), "B in team 0's view");
    let before = cs[0].snapshots.len();
    cs[0].move_to(-20.0, 0.0, 0.0, 0).await; // A keeps moving …
    cs[1].move_to(40.0, 0.0, 0.0, 0).await; // … B walks out of its vision
    arena.advance(&mut cs, 60).await;
    arena.flush(&mut cs).await;

    let raw = cs[0].snapshots[before..]
        .iter()
        .find(|raw| {
            let t = Typed::decode(&raw[..]).expect("typed decode");
            t.delta && !t.removed.is_empty() && !t.entities.is_empty()
        })
        .expect("a delta with a removal and an upsert")
        .clone();
    let k = kit::WorldSnapshot::decode(&raw[..]).expect("kit decode");
    let t = Typed::decode(&raw[..]).expect("typed decode");
    assert_eq!(
        (k.sequence, k.delta, &k.removed),
        (t.sequence, t.delta, &t.removed)
    );
    assert_eq!(t.removed, [cs[1].id], "B left team 0's view");
    assert!(t.entities.iter().all(|r| r.entity == cs[0].id), "A moved");
    let bodies: Vec<Vec<u8>> = t.entities.iter().map(Message::encode_to_vec).collect();
    assert_eq!(k.entities, bodies, "record bodies");
    let mut expected = kit::WorldSnapshot {
        sequence: t.sequence,
        delta: true,
        ..Default::default()
    }
    .encode_to_vec();
    for id in &t.removed {
        expected.push(0x18);
        prost::encoding::encode_varint(*id, &mut expected);
    }
    for body in &bodies {
        expected.push(0x12);
        prost::encoding::encode_varint(body.len() as u64, &mut expected);
        expected.extend_from_slice(body);
    }
    assert_eq!(&raw[..], &expected[..], "the kit's delta layout");
    assert_eq!(
        Typed::decode(&t.encode_to_vec()[..]).expect("re-decodes"),
        t
    );
    assert_eq!(cs[0].sees(), vec![cs[0].id], "the view followed");
}
