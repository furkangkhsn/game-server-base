//! The arena bot: its hand-walked record pinned to the generated
//! `UnitRecord` decoder, its home read off the session's `Welcome`, and
//! its inputs (silent until welcomed; then numbered, between home and
//! centre, with height).

use super::*;
use gsb_demo_arena::arena::{Private, UnitRecord, WorldSnapshot};

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
            let got = ArenaDecoder::default().record(&body).expect("hand decodes");
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
    assert_eq!(ArenaDecoder::default().record(&body), Ok((9, [-3, 4, 5])));
    for bad in [&[0x08][..], &[0x0A, 0x00][..], &[0x20, 0x80][..]] {
        assert!(UnitRecord::decode(bad).is_err(), "{bad:?}");
        assert!(ArenaDecoder::default().record(bad).is_err(), "{bad:?}");
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

/// The team room's deltas apply on the bot's view (the kit's client
/// rules): after a full, a delta removes the unit that left the team's
/// view and upserts the one that moved; a delta before any full (a late
/// joiner's first batch) is dropped, not an error.
#[test]
fn the_view_applies_the_team_rooms_deltas() {
    let unit = |entity, x| UnitRecord {
        entity,
        x,
        y: 0,
        z: 0,
    };
    let delta = WorldSnapshot {
        sequence: 8,
        entities: vec![unit(5, 2_700)],
        removed: vec![9],
        delta: true,
    }
    .encode_to_vec();
    let mut c = ArenaBot.client(0);
    c.apply_snapshot(&delta).expect("dropped, not an error");
    assert_eq!((c.view_len(), c.counters().gap_drops), (0, 1));
    let full = WorldSnapshot {
        sequence: 7,
        entities: vec![unit(5, 2_650), unit(9, 0)],
        removed: vec![],
        delta: false,
    };
    c.apply_snapshot(&full.encode_to_vec()).expect("a full");
    c.apply_snapshot(&delta).expect("a delta");
    let counters = c.counters();
    assert_eq!(
        (counters.fulls, counters.deltas, counters.errors),
        (1, 1, 0)
    );
    assert_eq!(c.view_len(), 1, "unit 9 left the view");
}

/// The session's first private frame: the welcome alone.
fn welcome(team: u32, teams: u32) -> Vec<u8> {
    Private {
        game: Some(Welcome { team, teams }),
        ..Default::default()
    }
    .encode_to_vec()
}

/// The welcome names the base: the arena's own formula (team 0 of three
/// on +x, team 1 at 120° toward +z), in centimetres; a team out of range
/// is an error that names no home.
#[test]
fn the_welcome_names_the_home() {
    let mut d = ArenaDecoder::default();
    d.session_private(&Welcome { team: 0, teams: 3 }.encode_to_vec())
        .expect("team 0");
    assert_eq!(d.home, Some([2_500, 0, 0]));
    d.session_private(&Welcome { team: 1, teams: 3 }.encode_to_vec())
        .expect("team 1");
    assert_eq!(d.home, Some([-1_250, 0, 2_165]));
    let mut bad = ArenaDecoder::default();
    for (team, teams) in [(3, 3), (0, 0), (300, 400)] {
        let body = Welcome { team, teams }.encode_to_vec();
        assert!(bad.session_private(&body).is_err(), "{team}/{teams}");
    }
    assert_eq!(bad.home, None);
}

/// No input before the welcome — seeing the own unit is not enough any
/// more; after it, one numbered MoveTo per call whose target runs from
/// the team's base to the centre and back and climbs between the floor
/// and 20 m.
#[test]
fn inputs_run_between_home_and_centre_once_welcomed() {
    let bot = ArenaBot;
    let mut c = bot.client(0);
    assert!(c.next_input(Duration::ZERO, 1).is_none(), "not welcomed");
    c.joined(5);
    c.apply_snapshot(&snapshot_with(5, [2_650, 0, 0]))
        .expect("applies");
    assert!(
        c.next_input(Duration::ZERO, 1).is_none(),
        "still not welcomed"
    );
    assert_eq!(c.apply_private(&welcome(0, 3)), Ok(PrivateEvent::Session));

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
