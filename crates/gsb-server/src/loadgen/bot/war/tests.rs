//! The war bot: its run records read by the war's own reader, the
//! faction read off the `Welcome`, its roster (every post held by all
//! three factions, spread over the four shards) and its input mix
//! (silent until welcomed and in view; then numbered, striking only an
//! enemy player within reach).

use bytes::BytesMut;
use prost::encoding::encode_varint;

use super::*;
use gsb_demo_war::codec::{WarWire, write_body};
use gsb_demo_war::components::Kind as UnitKind;
use gsb_demo_war::war::{Private, WorldSnapshot};
use gsb_demo_war::world::home_shard;

/// A war unit of the wire's `kind` (1 player, 2 tower, 3 point).
fn unit(x: i32, z: i32, kind: Kind, faction: u32, hp: u16) -> WarWire {
    WarWire {
        x,
        y: 0,
        z,
        kind: match kind {
            Kind::Tower => UnitKind::Tower,
            Kind::Point => UnitKind::Point,
            _ => UnitKind::Player,
        },
        faction: faction as u8,
        hp,
    }
}

/// Records written by the war's codec, back to back, read back through
/// the bot's decoder (as the kit's view calls it: id, then the body off
/// the front of the rest) — every field the bot keeps, and a truncated
/// body is an error.
#[test]
fn the_bot_reads_the_wars_run_records() {
    let coords = [0, 1, -1, 640, -641, 8_000, -8_000];
    let mut run = BytesMut::new();
    let mut want = Vec::new();
    for (i, x) in coords.into_iter().enumerate() {
        for (kind, faction, hp) in [
            (Kind::Player, 1, 100),
            (Kind::Tower, 3, 0),
            (Kind::Point, 0, 5),
        ] {
            let z = coords[(i + 3) % coords.len()];
            let w = unit(x, z, kind, faction, hp);
            encode_varint(i as u64 + 1, &mut run);
            write_body(&w, &mut run);
            want.push(WarRecord {
                x,
                z,
                kind: kind as i32,
                faction,
                hp: u32::from(hp),
            });
        }
    }
    let (dec, mut rest) = (WarDecoder::default(), &run[..]);
    for w in want {
        let _id = gsb_kit::client::wire::varint(&mut rest).expect("an id");
        assert_eq!(dec.run_record(0, &mut rest).expect("a record"), w);
    }
    assert!(rest.is_empty());
    assert!(dec.run_record(0, &mut &[0x12, 0xF5][..]).is_err());
}

/// Full snapshot `sequence` holding `(id, x, z, kind, faction)` records
/// (dm), in the war's record run.
fn full(sequence: u64, records: &[(u64, i32, i32, Kind, u32)]) -> Vec<u8> {
    let mut run = BytesMut::new();
    for &(entity, x, z, kind, faction) in records {
        encode_varint(entity, &mut run);
        write_body(&unit(x, z, kind, faction, 100), &mut run);
    }
    WorldSnapshot {
        sequence,
        records: run.to_vec(),
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
