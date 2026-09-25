//! The war bot: its hand-walked record pinned to the generated
//! `UnitRecord` decoder, the faction read off the `Welcome`, its roster
//! (every post held by all three factions, spread over the four shards)
//! and its input mix (silent until welcomed and in view; then numbered,
//! striking only an enemy player within reach).

use super::*;
use gsb_demo_war::war::{Private, UnitRecord, WorldSnapshot};
use gsb_demo_war::world::home_shard;

/// Every sample record decodes to what the generated decoder reads
/// (unknown fields skipped); what it rejects, the walk rejects.
#[test]
fn the_hand_walked_record_matches_the_generated_decoder() {
    let coords = [0, 1, -1, 640, -641, 8_000, -8_000, 120, i32::MAX, i32::MIN];
    for entity in [0u64, 1, 128, u64::MAX] {
        for (i, x) in coords.into_iter().enumerate() {
            for (kind, faction) in [(0, 0), (1, 1), (2, 3), (3, 0), (-1, 7)] {
                let (y, z) = (coords[(i + 3) % 10], coords[(i + 7) % 10]);
                let hp = (i as u32) * 25;
                let body = UnitRecord {
                    entity,
                    x,
                    y,
                    z,
                    kind,
                    faction,
                    hp,
                }
                .encode_to_vec();
                let t = UnitRecord::decode(&body[..]).expect("generated decodes");
                let (id, r) = WarDecoder::default().record(&body).expect("hand decodes");
                assert_eq!(
                    (id, r.x, r.z, r.kind, r.faction, r.hp),
                    (t.entity, t.x, t.z, t.kind, t.faction, t.hp)
                );
            }
        }
    }
    for bad in [&[0x08][..], &[0x0A, 0x00][..], &[0x38, 0x80][..]] {
        assert!(UnitRecord::decode(bad).is_err(), "{bad:?}");
        assert!(WarDecoder::default().record(bad).is_err(), "{bad:?}");
    }
}

/// Full snapshot `sequence` holding `(id, x, z, kind, faction)` records
/// (dm).
fn full(sequence: u64, records: &[(u64, i32, i32, Kind, u32)]) -> Vec<u8> {
    WorldSnapshot {
        sequence,
        entities: records
            .iter()
            .map(|&(entity, x, z, kind, faction)| UnitRecord {
                entity,
                x,
                y: 0,
                z,
                kind: kind as i32,
                faction,
                hp: 100,
            })
            .collect(),
        removed: vec![],
        delta: false,
    }
    .encode_to_vec()
}

fn welcome(faction: u32) -> Vec<u8> {
    Private {
        game: Some(Welcome {
            faction,
            factions: 3,
        }),
        ..Default::default()
    }
    .encode_to_vec()
}

/// Silent until welcomed and the own unit is in view; then every input
/// is numbered, and an attack goes only to the nearest enemy PLAYER
/// within 20 m — never an ally, a tower, or one out of reach.
#[test]
fn the_bot_strikes_only_an_enemy_player_in_reach() {
    let bot = WarBot {
        move_ms: ATTACK_EVERY, // one attack chance per input: certain
    };
    let mut c = bot.client(7);
    c.joined(10);
    assert!(c.next_input(Duration::ZERO, 1).is_none(), "not welcomed");
    c.apply_private(&welcome(2)).expect("welcome");
    assert!(c.next_input(Duration::ZERO, 1).is_none(), "not in view");
    c.apply_snapshot(&full(
        3,
        &[
            (10, 0, 0, Kind::Player, 2),
            (11, 50, 0, Kind::Player, 2),   // an ally, 5 m
            (12, 0, 150, Kind::Tower, 1),   // an enemy tower, 15 m
            (13, 0, 190, Kind::Player, 3),  // an enemy, 19 m
            (14, 120, 0, Kind::Player, 1),  // an enemy, 12 m — the nearest
            (15, 0, -210, Kind::Player, 1), // an enemy, 21 m
        ],
    ))
    .expect("full");
    let (op, body) = c.next_input(Duration::ZERO, 5).expect("an input");
    assert_eq!(op, op::WAR_ATTACK);
    assert_eq!(
        Attack::decode(&body[..]).expect("attack"),
        Attack { target: 14, seq: 5 }
    );

    c.apply_snapshot(&full(
        4,
        &[(10, 0, 0, Kind::Player, 2), (11, 50, 0, Kind::Player, 2)],
    ))
    .expect("full");
    let (op, body) = c.next_input(Duration::from_secs(1), 6).expect("an input");
    assert_eq!(op, op::WAR_MOVE_TO, "no enemy: the ring");
    assert_eq!(MoveTo::decode(&body[..]).expect("move").seq, 6);
    let zero = Welcome {
        faction: 0,
        factions: 3,
    };
    assert!(
        WarDecoder::default()
            .session_private(&zero.encode_to_vec())
            .is_err()
    );
}

/// The roster: faction `id mod 3` on post `(id / 3) mod 14`, so every
/// post is held by all three factions and every shard has characters;
/// a tower's ring stays within 45 m of its post, the middle point's
/// rings (70–95 m) run through all four regions.
#[test]
fn the_roster_spreads_every_faction_over_every_shard() {
    let mut shards = [[0u32; 3]; SHARDS];
    for id in 0..(3 * posts().len() as u64) {
        let at = roster::home(id);
        let f = roster::faction(id);
        shards[home_shard(&at)][usize::from(f.0)] += 1;
        let [px, pz] = posts()[roster::home_post(id)];
        let r = ((at.x - px).powi(2) + (at.z - pz).powi(2)).sqrt();
        assert!(r <= 95.01 && ([px, pz] == POINTS[0] || r <= 45.01), "{r}");
    }
    let middle = posts()
        .iter()
        .position(|p| *p == POINTS[0])
        .expect("a post");
    let mut regions = [false; SHARDS];
    for id in 0..6 {
        for t in 0..120 {
            let [x, z] = ring(middle, id, Duration::from_secs(t));
            regions[home_shard(&gsb_demo_war::Pos3::ground(x, z))] = true;
        }
    }
    assert_eq!(
        regions, [true; SHARDS],
        "the middle ring crosses both seams"
    );
    for (s, per) in shards.iter().enumerate() {
        assert!(per.iter().all(|&n| n >= 3), "shard {s}: {per:?}");
    }
    let realm = roster::realm();
    assert_eq!(realm.saved("lg-4").map(|s| s.faction), Some(Team(1)));
    for at in 0..posts().len() {
        let near = nearest_posts(at);
        assert!(!near.contains(&at) && near[0] != near[1] && near[1] != near[2]);
    }
}
