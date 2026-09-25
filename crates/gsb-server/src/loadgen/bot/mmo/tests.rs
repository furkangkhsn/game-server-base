//! The MMO bot: its hand-walked record pinned to the generated
//! `EntityRecord` decoder, and its input mix (silent until the own
//! character is seen; the K4 travel first; then roaming, travels and
//! attacks at their rates, all numbered).

use super::*;
use gsb_demo_mmo::mmo::{EntityRecord, WorldSnapshot};

/// Every sample record decodes to what the generated decoder reads
/// (unknown fields skipped); what it rejects, the walk rejects.
#[test]
fn the_hand_walked_record_matches_the_generated_decoder() {
    let coords = [
        0,
        1,
        -1,
        640,
        -641,
        5_120,
        -5_120,
        2_000,
        i32::MAX,
        i32::MIN,
    ];
    for entity in [0u64, 1, 128, u64::MAX] {
        for (i, x) in coords.into_iter().enumerate() {
            for kind in [0, 1, 2, 3, -1, 99] {
                let (y, z) = (coords[(i + 3) % 10], coords[(i + 7) % 10]);
                let hp = (i as u32) * 1_000;
                let body = EntityRecord {
                    entity,
                    x,
                    y,
                    z,
                    kind,
                    hp,
                }
                .encode_to_vec();
                let t = EntityRecord::decode(&body[..]).expect("generated decodes");
                let (id, r) = MmoDecoder.record(&body).expect("hand decodes");
                assert_eq!(
                    (id, r.x, r.y, r.z, r.kind),
                    (t.entity, t.x, t.y, t.z, t.kind)
                );
            }
        }
    }
    for bad in [
        &[0x08][..],
        &[0x0A, 0x00][..],
        &[0x28, 0x80][..],
        &[0x32, 0x05][..],
    ] {
        assert!(EntityRecord::decode(bad).is_err(), "{bad:?}");
        assert!(MmoDecoder.record(bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        MmoDecoder.cell_of(&MmoRecord {
            x: -1,
            y: 900,
            z: 640,
            kind: 1
        }),
        (-1, 1),
        "the ground cell, height ignored"
    );
}

/// A full snapshot holding `records` as `(id, x, y, z, kind)`.
pub(super) fn full(records: &[(u64, i32, i32, i32, Kind)]) -> Vec<u8> {
    WorldSnapshot {
        sequence: 3,
        entities: records
            .iter()
            .map(|&(entity, x, y, z, kind)| EntityRecord {
                entity,
                x,
                y,
                z,
                kind: kind as i32,
                hp: 100,
            })
            .collect(),
        removed: vec![],
        cell_exits: vec![],
        delta: false,
    }
    .encode_to_vec()
}

fn bot() -> MmoBot {
    MmoBot {
        move_ms: Duration::from_millis(150),
        duel_frac: 0.0,
    }
}

/// A client joined as `entity`, standing on waystone 0 with `extra`
/// records in view.
fn standing(id: u64, extra: &[(u64, i32, i32, i32, Kind)]) -> Box<dyn BotClient> {
    let mut c = bot().client(id);
    c.joined(7);
    let [wx, wz] = WAYSTONES[DEFAULT_WAYSTONE];
    let mut records = vec![(7, to_dm(wx), 0, to_dm(wz), Kind::Player)];
    records.extend_from_slice(extra);
    c.apply_snapshot(&full(&records)).expect("applies");
    c
}

/// Silent until the own character is in view; then the K4 travel to
/// waystone `id mod 4` first — except for the quarter already there.
#[test]
fn the_first_input_disperses_the_population() {
    let mut c = bot().client(1);
    assert!(c.next_input(Duration::ZERO, 1).is_none(), "not joined");
    c.joined(7);
    assert!(c.next_input(Duration::ZERO, 1).is_none(), "not in view");
    for id in 0..8u64 {
        let (op, payload) = standing(id, &[])
            .next_input(Duration::ZERO, 1)
            .expect("an input");
        if id % 4 == 0 {
            assert_eq!(op, op::MMO_MOVE_TO, "id {id} stays on waystone 0");
        } else {
            assert_eq!(op, op::MMO_TRAVEL, "id {id}");
            let t = Travel::decode(&payload[..]).expect("Travel");
            assert_eq!((t.waystone, t.seq), ((id % 4) as u32, 1));
        }
    }
}

/// Over many inputs the mix follows the rates: travels ≈ 150 ms / 20 s
/// of the inputs, to ANOTHER waystone; attacks only with a mob in reach
/// (≈ 150 ms / 1 s of the rest), at the nearest one; the rest roam the
/// ring round the current waystone; every input carries its seq.
#[test]
fn the_input_mix_follows_the_rates() {
    let [wx, wz] = WAYSTONES[DEFAULT_WAYSTONE];
    let (mx, mz) = (to_dm(wx + 20.0), to_dm(wz));
    let far = (to_dm(wx + 200.0), to_dm(wz));
    let mut c = standing(
        4,
        &[(50, mx, 0, mz, Kind::Mob), (51, far.0, 0, far.1, Kind::Mob)],
    );
    let (mut travels, mut attacks, mut moves) = (0, 0, 0);
    let mut at = DEFAULT_WAYSTONE;
    for seq in 1..=20_000u64 {
        let t = Duration::from_millis(seq * 150);
        let (op, payload) = c.next_input(t, seq).expect("an input");
        match op {
            op::MMO_TRAVEL => {
                let m = Travel::decode(&payload[..]).expect("Travel");
                assert_eq!(m.seq, seq);
                assert_ne!(m.waystone as usize, at, "another waystone");
                at = m.waystone as usize;
                travels += 1;
            }
            op::MMO_ATTACK => {
                let m = Attack::decode(&payload[..]).expect("Attack");
                assert_eq!((m.target, m.seq), (50, seq), "the mob in reach");
                attacks += 1;
            }
            _ => {
                let m = MoveTo::decode(&payload[..]).expect("MoveTo");
                assert_eq!(m.seq, seq);
                let [ax, az] = WAYSTONES[at];
                let r = f64::from(m.x - to_dm(ax)).hypot(f64::from(m.z - to_dm(az)));
                assert!((299.0..=601.0).contains(&r), "on the ring: {r} dm");
                moves += 1;
            }
        }
    }
    assert!((100..=200).contains(&travels), "travels {travels}");
    assert!((2_400..=3_600).contains(&attacks), "attacks {attacks}");
    assert_eq!(travels + attacks + moves, 20_000);
}

/// The flood input is unnumbered; the churn input carries its seq.
#[test]
fn flood_is_unnumbered_and_churn_is_numbered() {
    let (op, flood) = bot().flood_input();
    assert_eq!(op, op::MMO_MOVE_TO);
    assert_eq!(MoveTo::decode(&flood[..]).expect("MoveTo").seq, 0);
    let (_, churn) = bot().churn_input(3, 42);
    assert_eq!(MoveTo::decode(&churn[..]).expect("MoveTo").seq, 42);
}
