//! The full mode (the default) is the team room's pre-delta output,
//! byte for byte: over the seeded random run every team frame is what
//! the room's previous inline encoder wrote for the same content (the
//! same "unchanged ⇒ nothing" decision, the full header, the records in
//! the content's order), the keep-alive keeps the core's default re-send
//! and no private frame ever carries a one-shot full. Plus one frame
//! pinned as literal bytes.

use std::collections::HashMap;

use super::run::{Script, Twin};
use super::*;
use crate::common::{put_entity_records, write_full_header};
use crate::testing::{FixCodec, WirePos};

/// The pre-delta encoder of one team's frame (the room's `snapshot`
/// body before this round), over the room's own content cache.
fn previous_encoder(
    last: &mut HashMap<Team, HashMap<u64, WirePos>>,
    team: Team,
    content: &HashMap<u64, WirePos>,
    tick: u64,
) -> Option<bytes::BytesMut> {
    let last = last.entry(team).or_default();
    if *last == *content {
        return None;
    }
    let mut out = bytes::BytesMut::new();
    write_full_header(&mut out, tick);
    put_entity_records(
        &FixCodec,
        content.iter().map(|(id, wire)| (*id, wire)),
        &mut out,
    );
    *last = content.clone();
    Some(out)
}

#[test]
fn full_mode_frames_are_the_pre_delta_frames() {
    let mut script = Script::new(0xF011_0417);
    let mut twin = Twin::new(TeamRoom::new(25.0));
    let mut last = HashMap::new();
    let mut frames = 0;
    for tick in 1..=600u64 {
        for op in script.tick() {
            twin.apply(op);
        }
        let (world, room) = (&mut twin.world, &mut twin.room);
        room.update(world, &ctx(tick));
        let groups: Vec<(PlayerId, Team)> = twin
            .players
            .iter()
            .map(|&p| (p, room.group_of(world, p)))
            .collect();
        let teams: BTreeSet<u8> = groups.iter().map(|&(_, t)| t.0).collect();
        for team in teams.into_iter().map(Team) {
            let content = &room.contents[usize::from(team.0)];
            let expected = previous_encoder(&mut last, team, content, tick);
            let mut out = bytes::BytesMut::new();
            let emitted = room.snapshot(world, &ctx(tick), &team, &[], &mut out);
            assert_eq!(emitted, expected.is_some(), "tick {tick}, team {team:?}");
            assert_eq!(out, expected.unwrap_or_default(), "tick {tick}: the bytes");
            frames += usize::from(emitted);
            out.clear();
            assert!(!room.keepalive(world, &ctx(tick), &team, None, &mut out));
            assert!(out.is_empty(), "the core's default re-send");
        }
        for (p, team) in groups {
            let mut out = bytes::BytesMut::new();
            if room.private(world, p, &team, &[], &mut out) {
                let private = crate::testing::Private::decode(out.as_ref()).expect("private");
                assert!(
                    !matches!(
                        private.payload,
                        Some(crate::testing::private::Payload::Snapshot(_))
                    ),
                    "no one-shot full in full mode"
                );
            }
        }
    }
    assert!(frames > 600, "the run emitted: {frames}");
}

/// One team frame as literal bytes: sequence 5, one record (wire id 1
/// at (7, 9)): `08 05` + `12 06` + `08 01 10 0e 18 12`.
#[test]
fn a_full_mode_frame_pinned_byte_for_byte() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);
    place(&mut world, &mut room, ConnectionId(2), 7.9, 9.2);
    room.update(&mut world, &ctx(5));
    let mut out = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(5), &Team(0), &[], &mut out));
    assert_eq!(
        &out[..],
        &[0x08, 0x05, 0x12, 0x06, 0x08, 0x01, 0x10, 0x0e, 0x18, 0x12]
    );
}
