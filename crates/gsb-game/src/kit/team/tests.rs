//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

//! Logic-level team-fog tests (precise, direct `TeamRoom` calls; they
//! need the room's private bookkeeping, so they live here rather than
//! in `tests/team.rs`). `vision_radius = 25`.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::kit::seam::{DEFAULT_SPEED, Position, Speed};
use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::room::TickCtx;

use super::*;
use gsb_core::id::PlayerId;
use gsb_core::room::GameLogic;
use prost::Message;

/// The instantiation these tests drive: the demo game over the kit's 2D
/// vision preset (shadows the generic room of `use super::*`).
type TeamRoom =
    super::TeamRoom<crate::kit::seam::DemoGame, crate::kit::space::VisionGrid2<Position>>;

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

/// Join a player (wire id assigned; team = conn % 2) and move its
/// entity to an exact position. Returns the wire id.
/// Place a player; returns `(wire id, stable player id)` — the tests
/// address entities through the pid their join actually minted (the
/// minting counter, NOT the conn number: joins below are deliberately
/// in non-conn order for team parity).
fn place(
    world: &mut World,
    room: &mut TeamRoom,
    conn: ConnectionId,
    x: f32,
    y: f32,
) -> (u64, PlayerId) {
    let admission = room.on_join(world, conn);
    let entity = *room
        .player_entity
        .get(&admission.player)
        .expect("registered");
    world.entity_mut(entity).insert(Position { x, y });
    (admission.entity, admission.player)
}

fn snap_ids(out: &bytes::BytesMut) -> BTreeSet<u64> {
    crate::kit::seam::WorldSnapshot::decode(out.as_ref())
        .expect("snapshot payload")
        .entities
        .iter()
        .map(|e| e.entity)
        .collect()
}

/// The spec's cheat test at the logic level (same tick): an enemy
/// outside team 0's vision is ABSENT from team 0's snapshot while
/// PRESENT in team 1's snapshot — information not shipped is not
/// merely hard to decode, it is not in the bytes.
#[test]
fn enemy_out_of_vision_absent_in_one_team_present_in_other() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);

    // A: conn 2 → team 0, at (0,0).
    // B: conn 1 → team 1, at (10,0)  (10 < 25: in A's team's vision).
    // C: conn 3 → team 1, at (40,0)  (40 > 25: outside A's team's vision;
    //                                 team 1's own package always has C).
    let (a, _) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
    let (b, _) = place(&mut world, &mut room, ConnectionId(1), 10.0, 0.0);
    let (c, _) = place(&mut world, &mut room, ConnectionId(3), 40.0, 0.0);
    room.update(&mut world, &ctx(1));

    // Team 0: own team {A} + in-vision enemies {B} → {A, B}; NOT C.
    let mut out0 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &[], &mut out0));
    let t0 = snap_ids(&out0);
    assert!(
        t0.contains(&a) && t0.contains(&b),
        "team 0 sees A,B: {t0:?}"
    );
    assert!(!t0.contains(&c), "C is out of team 0's vision: {t0:?}");

    // Team 1 (same tick): own team {B, C} + in-vision enemies {A}
    // (10 < 25) → {A, B, C}. C is in team 1's snapshot on the SAME
    // tick it is absent from team 0's.
    let mut out1 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &[], &mut out1));
    let t1 = snap_ids(&out1);
    assert!(
        t1.contains(&a) && t1.contains(&b) && t1.contains(&c),
        "team 1: {t1:?}"
    );
}

/// Own team is always visible, with NO range limit (400+ units away),
/// and the range limit applies only to *enemy* visibility.
#[test]
fn own_team_always_visible_regardless_of_distance() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);

    let (a, _) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0); // team 0
    let (d, _) = place(&mut world, &mut room, ConnectionId(4), 400.0, 400.0); // team 0, far
    let (b, _) = place(&mut world, &mut room, ConnectionId(1), 10.0, 0.0); // team 1

    room.update(&mut world, &ctx(1));

    // Team 0's snapshot has both own-team members (D is ~566 from A).
    let mut out0 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &[], &mut out0));
    let t0 = snap_ids(&out0);
    assert!(
        t0.contains(&a) && t0.contains(&d),
        "own team is always in the package: {t0:?}"
    );
    assert!(t0.contains(&b), "B is within 25 of A: {t0:?}");

    // Team 1's snapshot has B but NOT D (D is far from every team-1 unit).
    let mut out1 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &[], &mut out1));
    let t1 = snap_ids(&out1);
    assert!(t1.contains(&b), "team 1 sees itself: {t1:?}");
    assert!(!t1.contains(&d), "D is out of team 1's vision: {t1:?}");
}

/// The previously untested corner: an enemy entering vision arrives
/// with the SAME wire identity it already had (identity does not
/// change on a visibility transition).
#[test]
fn enemy_entering_vision_keeps_its_wire_identity() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);

    let (a, _) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0); // team 0
    let (b, pb) = place(&mut world, &mut room, ConnectionId(1), 100.0, 100.0); // team 1, far
    room.update(&mut world, &ctx(1));

    // Tick 1: B is in team 1's own package (id known to B's team's
    // clients) and out of team 0's vision.
    let mut out1 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &[], &mut out1));
    let t1 = snap_ids(&out1);
    assert!(t1.contains(&b), "B in own package: {t1:?}");
    let mut out0 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &[], &mut out0));
    assert!(!snap_ids(&out0).contains(&b), "B out of vision: {out0:?}");

    // B moves into A's vision: (5,0), distance 5 < 25.
    let entity_b = *room.player_entity.get(&pb).unwrap();
    world
        .entity_mut(entity_b)
        .insert(Position { x: 5.0, y: 0.0 });
    room.update(&mut world, &ctx(2));

    let mut out0b = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(2), &Team(0), &[], &mut out0b),
        "vision change re-emits"
    );
    let t0 = snap_ids(&out0b);
    assert!(
        t0.contains(&b),
        "B now in team 0's package — with the SAME wire id it had in \
         tick 1 (identity did not change on the visibility transition): {t0:?}"
    );
    assert!(t0.contains(&a));
}

/// An entity leaving vision DROPS OUT of the snapshot, and the
/// snapshot stays self-contained: the client reads "gone" from the
/// full replacement alone (no delta, no history, no remove event).
#[test]
fn enemy_leaving_vision_drops_from_snapshot_self_contained() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);

    let (a, _) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
    let (b, pb) = place(&mut world, &mut room, ConnectionId(1), 5.0, 0.0); // in vision
    room.update(&mut world, &ctx(1));

    let mut out0 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &[], &mut out0));
    let t0 = snap_ids(&out0);
    assert!(t0.contains(&a) && t0.contains(&b), "B in vision: {t0:?}");

    // B leaves vision.
    let entity_b = *room.player_entity.get(&pb).unwrap();
    world
        .entity_mut(entity_b)
        .insert(Position { x: 300.0, y: 300.0 });
    room.update(&mut world, &ctx(2));

    let mut out0b = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(2), &Team(0), &[], &mut out0b),
        "vision change re-emits"
    );
    let snap = crate::kit::seam::WorldSnapshot::decode(out0b.as_ref()).expect("snapshot");
    let t0b: BTreeSet<u64> = snap.entities.iter().map(|e| e.entity).collect();
    assert!(!t0b.contains(&b), "B dropped out: {t0b:?}");
    assert_eq!(
        t0b,
        [a].into_iter().collect(),
        "exactly {a} remains — no stale record"
    );
    assert_eq!(snap.sequence, 2, "sequence advanced (order/duplicate-safe)");

    // B's own team still has B (own-team visibility is unconditional).
    let mut out1b = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(2), &Team(1), &[], &mut out1b));
    assert!(snap_ids(&out1b).contains(&b), "B still in own package");
}

/// Broadcast set (structural, like the other rooms): an entity
/// spawned with a `Position` but no `WireId` (a bullet/ward — no
/// connection, hence no team) is stamped in `update` and appears in
/// BOTH teams' snapshots (neutral ⇒ broadcast to all teams).
#[test]
fn neutral_entity_is_broadcast_to_all_teams() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);

    let (a, _) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0); // team 0
    let (b, _) = place(&mut world, &mut room, ConnectionId(1), 5.0, 0.0); // team 1, 5 from A
    let _orphan = world
        .spawn((Position { x: 2.0, y: 2.0 }, Speed(DEFAULT_SPEED)))
        .id();
    room.update(&mut world, &ctx(1));

    let mut out0 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &[], &mut out0));
    let t0 = snap_ids(&out0);
    let mut out1 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &[], &mut out1));
    let t1 = snap_ids(&out1);

    // B is within 25 of A, so BOTH teams see both players; the neutral
    // orphan goes to both packages as well.
    assert_eq!(
        t0,
        [a, b, 3].into_iter().collect(),
        "team 0: A, B (in vision), neutral: {t0:?}"
    );
    assert_eq!(
        t1,
        [a, b, 3].into_iter().collect(),
        "team 1: B, A (in vision), neutral: {t1:?}"
    );
    assert!(
        t0.contains(&3) && t1.contains(&3),
        "neutral stamped with fresh serial 3"
    );
}

/// "No change" contract: with a static world (no movement targets),
/// each team's second snapshot is silent — and the two teams'
/// ledgers are independent (one team's emission does not consume the
/// other team's change).
#[test]
fn team_no_change_when_static_and_ledgers_independent() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);

    let (_, pmover) = place(&mut world, &mut room, ConnectionId(2), 7.0, 9.0); // team 0, no MoveTarget
    // Team 1 far away: the teams do NOT see each other's units
    // (193 > 25), so only the mover's team re-emits below.
    place(&mut world, &mut room, ConnectionId(1), 200.0, 0.0); // team 1
    room.update(&mut world, &ctx(1));

    // First emission: both changed (membership).
    let mut o = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &[], &mut o));
    assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &[], &mut o));

    // Second: both silent, regardless of order.
    room.update(&mut world, &ctx(2));
    let mut o2 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx(2), &Team(1), &[], &mut o2),
        "team 1 silent"
    );
    assert!(
        !room.snapshot(&mut world, &ctx(2), &Team(0), &[], &mut o2),
        "team 0 silent"
    );
    assert!(o2.is_empty(), "no bytes written on silence");

    // A movement in ONE team re-emits that team only.
    let entity = *room.player_entity.get(&pmover).unwrap();
    world
        .entity_mut(entity)
        .insert(Position { x: 8.0, y: 19.0 });
    room.update(&mut world, &ctx(3));
    let mut o3 = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(3), &Team(0), &[], &mut o3),
        "mover's team re-emits"
    );
    let mut o4 = bytes::BytesMut::new();
    assert!(
        !room.snapshot(&mut world, &ctx(3), &Team(1), &[], &mut o4),
        "other team still silent"
    );
}

/// Item D, pinned: team membership is WORLD STATE (the
/// [`TeamMember`] component), so a connection that changes team at
/// runtime moves its snapshot group on the next re-evaluation — and
/// the wire identity survives the transition (a group transition is
/// not an identity transition, the same invariant the cell-crossing
/// and vision-transition tests pin for the other strategies).
///
/// Layout (vision radius 25): A(0,0) and A2(30,0) are team 0 (conn
/// parity); B(100,0) and B2(200,0) are team 1. The teams start out of
/// each other's vision entirely (A↔B is 100, A2↔B is 70, both > 25),
/// so before the switch each package is exactly its own team — which
/// also makes both transitions below visible in the bytes (if A were
/// already in team 1's vision as an enemy, team 1's package would be
/// byte-identical before and after: snapshots carry records, not
/// "role" — the group move still happens, only less visibly).
/// - Before: team 0 = {A, A2}; team 1 = {B, B2}.
/// - A switches to team 1 (a plain component write — the game rule
///   that causes the switch is outside the seam; a future trade op
///   would do exactly this write).
/// - After: team 1 = own {A, B, B2}; the enemy A2 is 30 from A (> 25)
///   and 70 from B ⇒ A *appears* in team 1's package (own-team
///   visibility has no range limit) with the SAME wire id minted at
///   its join. Team 0 = own {A2}; A is now an enemy 30 from A2
///   (> 25) ⇒ A *leaves* team 0's package.
#[test]
fn runtime_team_change_moves_the_group_and_keeps_the_wire_identity() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);

    let (a, pa) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0); // team 0
    let (a2, _) = place(&mut world, &mut room, ConnectionId(4), 30.0, 0.0); // team 0
    let (b, pb) = place(&mut world, &mut room, ConnectionId(1), 100.0, 0.0); // team 1
    let (b2, _) = place(&mut world, &mut room, ConnectionId(3), 200.0, 0.0); // team 1
    room.update(&mut world, &ctx(1));

    // Baseline: group_of reads the world's TeamMember (parity at join
    // time), and each package is exactly its own team.
    assert_eq!(room.group_of(&world, pa), Team(0));
    assert_eq!(room.group_of(&world, pb), Team(1));
    let mut out0 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &[], &mut out0));
    let t0 = snap_ids(&out0);
    let mut out1 = bytes::BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(1), &[], &mut out1));
    let t1 = snap_ids(&out1);
    assert_eq!(t0, [a, a2].into_iter().collect(), "team 0: {t0:?}");
    assert_eq!(t1, [b, b2].into_iter().collect(), "team 1: {t1:?}");

    // RUNTIME TEAM CHANGE: A (conn 2) switches to team 1. This is a
    // component write on world state — no room hook, no protocol op.
    let entity_a = *room.player_entity.get(&pa).unwrap();
    world.entity_mut(entity_a).insert(TeamMember(Team(1)));

    // The player's group follows on the re-evaluation — the seam
    // question, pinned: group_of reads the world, and the world says
    // team 1 now.
    assert_eq!(
        room.group_of(&world, pa),
        Team(1),
        "A's group moved with its TeamMember"
    );

    room.update(&mut world, &ctx(2));
    let mut out0b = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(2), &Team(0), &[], &mut out0b),
        "team 0 re-emits"
    );
    let mut out1b = bytes::BytesMut::new();
    assert!(
        room.snapshot(&mut world, &ctx(2), &Team(1), &[], &mut out1b),
        "team 1 re-emits"
    );
    let t0b = snap_ids(&out0b);
    let t1b = snap_ids(&out1b);

    // A LEFT team 0's package (it is now an enemy 30 from A2, outside
    // team 0's vision): the group transition is visible in the bytes.
    assert_eq!(
        t0b,
        [a2].into_iter().collect(),
        "team 0: A is gone: {t0b:?}"
    );

    // A APPEARED in team 1's package as OWN TEAM — with the SAME wire
    // id it had before the transition (identity did not change on the
    // group transition).
    assert!(
        t1b.contains(&a),
        "A is in team 1's package with the pre-transition wire id: {t1b:?}"
    );
    assert_eq!(t1b, [a, b, b2].into_iter().collect(), "team 1: {t1b:?}");
}
