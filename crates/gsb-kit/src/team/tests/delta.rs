//! The delta mode ([`TeamRoom::with_delta`]), rule by rule at the logic
//! level: a team's frame is exactly the records that left its view
//! (`removed`) plus those that entered it or changed on the wire
//! (upserts); a fresh team gets a full; the keep-alive ships a fresh
//! full; a member without a baseline gets a one-shot private full.
//! Vision radius 25; teams by conn parity (conn 2, 4 → team 0; conn 1,
//! 3 → team 1).

use super::*;
use crate::testing::{Private, WorldSnapshot, private::Payload};

mod convergence;
mod full_only;
mod one_shot;
mod run;

fn delta_room() -> TeamRoom {
    TeamRoom::new(25.0).with_delta()
}

fn decode(out: &bytes::BytesMut) -> WorldSnapshot {
    WorldSnapshot::decode(out.as_ref()).expect("snapshot payload")
}

/// `(wire id, x, y)` of every record in the frame.
fn records(s: &WorldSnapshot) -> BTreeSet<(u64, i32, i32)> {
    s.entities.iter().map(|r| (r.entity, r.x, r.y)).collect()
}

/// The team's frame this tick (`None`: silent).
fn frame(room: &mut TeamRoom, world: &mut World, tick: u64, team: u8) -> Option<WorldSnapshot> {
    let mut out = bytes::BytesMut::new();
    room.snapshot(world, &ctx(tick), &Team(team), &[], &mut out)
        .then(|| decode(&out))
}

/// The player's private frame this tick (`None`: no frame).
fn private(room: &mut TeamRoom, world: &mut World, player: PlayerId, team: u8) -> Option<Private> {
    let mut out = bytes::BytesMut::new();
    room.private(world, player, &Team(team), &[], &mut out)
        .then(|| Private::decode(out.as_ref()).expect("private payload"))
}

/// The one-shot full a private frame carries, if any.
fn one_shot(p: &Option<Private>) -> Option<WorldSnapshot> {
    match p.as_ref()?.payload.as_ref()? {
        Payload::Snapshot(s) => Some(s.clone()),
        Payload::Ack(_) => None,
    }
}

fn move_to(world: &mut World, room: &TeamRoom, player: PlayerId, x: f32, y: f32) {
    let entity = room.player_entity[&player];
    world.entity_mut(entity).insert(Position { x, y });
}

/// An enemy entering a team's vision is an upsert; leaving it is a
/// `removed` entry — and nothing else is in either frame (the team's own
/// static unit is not re-sent).
#[test]
fn entering_vision_is_an_upsert_and_leaving_is_a_removal() {
    let mut world = World::new();
    let mut room = delta_room();
    let (a, _) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0); // team 0
    let (b, pb) = place(&mut world, &mut room, ConnectionId(1), 100.0, 0.0); // team 1
    room.update(&mut world, &ctx(1));
    assert!(!frame(&mut room, &mut world, 1, 0).expect("full").delta);
    assert!(!frame(&mut room, &mut world, 1, 1).expect("full").delta);

    // B walks into A's vision (10 < 25): team 0 gains B, team 1 gains A
    // and B's own move.
    move_to(&mut world, &room, pb, 10.0, 0.0);
    room.update(&mut world, &ctx(2));
    let s0 = frame(&mut room, &mut world, 2, 0).expect("team 0 changed");
    assert!(s0.delta && s0.removed.is_empty());
    assert_eq!(records(&s0), [(b, 10, 0)].into_iter().collect(), "B enters");
    let s1 = frame(&mut room, &mut world, 2, 1).expect("team 1 changed");
    assert!(s1.delta && s1.removed.is_empty());
    assert_eq!(
        records(&s1),
        [(a, 0, 0), (b, 10, 0)].into_iter().collect(),
        "A enters, B moved"
    );

    // B walks away: team 0 removes it; team 1 removes A, upserts B.
    move_to(&mut world, &room, pb, 300.0, 300.0);
    room.update(&mut world, &ctx(3));
    let s0 = frame(&mut room, &mut world, 3, 0).expect("team 0 changed");
    assert!(s0.delta);
    assert_eq!(s0.removed, [b], "B left team 0's vision");
    assert!(s0.entities.is_empty(), "nothing else: {s0:?}");
    let s1 = frame(&mut room, &mut world, 3, 1).expect("team 1 changed");
    assert_eq!(s1.removed, [a], "A left team 1's vision");
    assert_eq!(records(&s1), [(b, 300, 300)].into_iter().collect());
    assert!(s0.cell_exits.is_empty() && s1.cell_exits.is_empty());
}

/// A visible unit that moves is an upsert only when its WIRE value
/// changes (the codec truncates): a sub-unit move is silent.
#[test]
fn a_moving_unit_is_upserted_only_when_its_wire_value_changes() {
    let mut world = World::new();
    let mut room = delta_room();
    let (a, pa) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
    place(&mut world, &mut room, ConnectionId(4), 5.0, 5.0);
    room.update(&mut world, &ctx(1));
    frame(&mut room, &mut world, 1, 0).expect("full");

    move_to(&mut world, &room, pa, 0.4, 0.9);
    room.update(&mut world, &ctx(2));
    assert!(
        frame(&mut room, &mut world, 2, 0).is_none(),
        "same wire value: silent"
    );

    move_to(&mut world, &room, pa, 1.2, 0.9);
    room.update(&mut world, &ctx(3));
    let s = frame(&mut room, &mut world, 3, 0).expect("the wire value changed");
    assert!(s.delta && s.removed.is_empty());
    assert_eq!(records(&s), [(a, 1, 0)].into_iter().collect(), "A only");
    assert_eq!(room.encoded_records(), 3, "two in the full, one upsert");
}

/// On the keep-alive cadence a team ships a FRESH full of its view,
/// active or silent (the convergence guarantee) — and the ledger follows
/// it, so the next unchanged tick is silent. The full mode keeps the
/// core's default re-send.
#[test]
fn the_keepalive_ships_a_fresh_full() {
    let mut world = World::new();
    let mut room = delta_room();
    let (a, pa) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
    room.update(&mut world, &ctx(1));
    frame(&mut room, &mut world, 1, 0).expect("full");

    move_to(&mut world, &room, pa, 3.0, 0.0);
    room.update(&mut world, &ctx(2));
    assert!(frame(&mut room, &mut world, 2, 0).expect("delta").delta);
    let mut out = bytes::BytesMut::new();
    assert!(room.keepalive(&mut world, &ctx(2), &Team(0), None, &mut out));
    let full = decode(&out);
    assert!(!full.delta && full.sequence == 2, "a fresh full: {full:?}");
    assert_eq!(records(&full), [(a, 3, 0)].into_iter().collect());

    // A silent team: the keep-alive is a full all the same.
    room.update(&mut world, &ctx(3));
    assert!(frame(&mut room, &mut world, 3, 0).is_none());
    let mut out = bytes::BytesMut::new();
    assert!(room.keepalive(&mut world, &ctx(3), &Team(0), None, &mut out));
    assert!(!decode(&out).delta);
    room.update(&mut world, &ctx(4));
    assert!(frame(&mut room, &mut world, 4, 0).is_none(), "still silent");

    let mut full_mode = TeamRoom::new(25.0);
    let mut world = World::new();
    place(&mut world, &mut full_mode, ConnectionId(2), 0.0, 0.0);
    full_mode.update(&mut world, &ctx(1));
    let mut out = bytes::BytesMut::new();
    full_mode.snapshot(&mut world, &ctx(1), &Team(0), &[], &mut out);
    out.clear();
    assert!(
        !full_mode.keepalive(&mut world, &ctx(1), &Team(0), None, &mut out),
        "full mode: the core's default re-send"
    );
    assert!(out.is_empty());
}

/// A team whose group was not asked for on the previous step (it had no
/// members) is fresh again: its next frame is a full.
#[test]
fn a_team_group_reborn_after_a_gap_gets_a_full() {
    let mut world = World::new();
    let mut room = delta_room();
    let (a, pa) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
    room.update(&mut world, &ctx(1));
    frame(&mut room, &mut world, 1, 0).expect("full");
    room.update(&mut world, &ctx(2)); // the group had no members
    move_to(&mut world, &room, pa, 7.0, 0.0);
    room.update(&mut world, &ctx(3));
    let s = frame(&mut room, &mut world, 3, 0).expect("reborn");
    assert!(!s.delta, "a reborn group's frame is a full");
    assert_eq!(records(&s), [(a, 7, 0)].into_iter().collect());
}
