//! The arena bot: its hand-walked record pinned to the generated
//! `UnitRecord` decoder, and its inputs (silent until the own unit is
//! seen; then numbered, between home and centre, with height).

use super::*;
use gsb_demo_arena::arena::{UnitRecord, WorldSnapshot};

/// Every sample id × coordinate decodes to what the generated decoder
/// reads (unknown fields skipped); what it rejects, the walk rejects.
#[test]
fn the_hand_walked_record_matches_the_generated_decoder() {
    let coords = [0, 1, -1, 63, -64, 5_000, -5_000, 3_000, i32::MAX, i32::MIN];
    for entity in [0u64, 1, 128, u64::MAX] {
        for (i, x) in coords.into_iter().enumerate() {
            let (y, z) = (coords[(i + 3) % 10], coords[(i + 7) % 10]);
            let body = UnitRecord { entity, x, y, z }.encode_to_vec();
            let typed = UnitRecord::decode(&body[..]).expect("generated decodes");
            let got = ArenaDecoder.record(&body).expect("hand decodes");
            assert_eq!(got, (typed.entity, [typed.x, typed.y, typed.z]));
        }
    }
    let mut body = UnitRecord {
        entity: 9,
        x: -3,
        y: 4,
        z: 5,
    }
    .encode_to_vec();
    body.extend_from_slice(&[0x2A, 0x01, 0xFF]); // field 5, bytes
    assert_eq!(ArenaDecoder.record(&body), Ok((9, [-3, 4, 5])));
    for bad in [&[0x08][..], &[0x0A, 0x00][..], &[0x20, 0x80][..]] {
        assert!(UnitRecord::decode(bad).is_err(), "{bad:?}");
        assert!(ArenaDecoder.record(bad).is_err(), "{bad:?}");
    }
}

/// A team snapshot holding the client's own unit at `at` (cm).
fn snapshot_with(entity: u64, at: [i32; 3]) -> Vec<u8> {
    WorldSnapshot {
        sequence: 7,
        entities: vec![UnitRecord {
            entity,
            x: at[0],
            y: at[1],
            z: at[2],
        }],
        removed: vec![],
        delta: false,
    }
    .encode_to_vec()
}

/// No input before the own unit is in view; after, one numbered MoveTo
/// per call whose target runs from home to the centre and back and
/// climbs between the floor and 20 m.
#[test]
fn inputs_run_between_home_and_centre_once_home_is_seen() {
    let bot = ArenaBot;
    let mut c = bot.client(0);
    assert!(c.next_input(Duration::ZERO, 1).is_none(), "not joined");
    c.joined(5);
    assert!(c.next_input(Duration::ZERO, 1).is_none(), "own unit unseen");
    let home = [2_500, 0, 0];
    c.apply_snapshot(&snapshot_with(5, home)).expect("applies");

    let mut seen_home = false;
    let mut seen_centre = false;
    let (mut low, mut high) = (i32::MAX, i32::MIN);
    for (n, tenth) in (0..80).enumerate() {
        let t = Duration::from_millis(tenth * 100);
        let seq = n as u64 + 1;
        let (op, payload) = c.next_input(t, seq).expect("an input");
        assert_eq!(op, gsb_demo_arena::op::ARENA_MOVE_TO);
        let m = MoveTo::decode(&payload[..]).expect("a MoveTo");
        assert_eq!(m.seq, seq, "numbered as given");
        assert!((0..=2_500).contains(&m.x) && m.z == 0, "on the line: {m:?}");
        seen_home |= m.x > 2_400;
        seen_centre |= m.x < 100;
        (low, high) = (low.min(m.y), high.max(m.y));
    }
    assert!(seen_home && seen_centre, "a full round trip in 8 s");
    assert!(low < 200 && high > 1_800, "height swings: {low}..{high}");
}

/// The flood input is unnumbered; the churn input carries its seq.
#[test]
fn flood_is_unnumbered_and_churn_is_numbered() {
    let (op, flood) = ArenaBot.flood_input();
    assert_eq!(op, gsb_demo_arena::op::ARENA_MOVE_TO);
    assert_eq!(MoveTo::decode(&flood[..]).expect("MoveTo").seq, 0);
    let (_, churn) = ArenaBot.churn_input(3, 42);
    assert_eq!(MoveTo::decode(&churn[..]).expect("MoveTo").seq, 42);
}
