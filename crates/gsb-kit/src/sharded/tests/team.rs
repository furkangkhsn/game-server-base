//! The `team × sharded` composite driven directly (one shard, the TEAMS
//! phase called by hand with a hand-built borrowed set and imports):
//! what a shard exports per team, what each viewed team's content holds,
//! the precedence own > lent > imported, and the team across a
//! migration. The real actors and the registry hub: `team_actors`.

use bytes::{Bytes, BytesMut};
use gsb_core::shard::{TeamExport, TeamImport, TeamImports, TeamRecord};

use super::*;
use crate::codec::RecordCodec;
use crate::team::{Team, TeamMember};

mod migrate;
mod vision;

/// Vision radius of every test here.
const R: f32 = 25.0;

type TeamShard = super::ShardedTeamRoom<
    crate::testing::Fixture,
    crate::space::GridPartition2<Position>,
    crate::space::VisionGrid2<Position>,
>;

/// Shard 0 of a 2×2 grid over `[-100, 100]²` (region x < 0, y < 0).
fn shard0() -> TeamShard {
    TeamShard::new(0, 4, 100.0, R)
}

/// Join a player on `team` at `(x, y)`; returns its wire id.
fn member(world: &mut World, room: &mut TeamShard, conn: u64, team: u8, x: f32, y: f32) -> u64 {
    let a = room.on_join(world, ConnectionId(conn));
    let e = room.inner.player_entity[&a.player];
    world
        .entity_mut(e)
        .insert((Position { x, y }, TeamMember(Team(team))));
    a.entity
}

/// Spawn an NPC unit (a ward, `team: None` = neutral) at `(x, y)`; the
/// next update stamps its wire id. Returns the entity.
fn npc(world: &mut World, team: Option<u8>, x: f32, y: f32) -> bevy_ecs::prelude::Entity {
    let e = world.spawn(Position { x, y }).id();
    if let Some(t) = team {
        world.entity_mut(e).insert(TeamMember(Team(t)));
    }
    e
}

fn wire_of(world: &World, e: bevy_ecs::prelude::Entity) -> u64 {
    world.get::<WireId>(e).expect("stamped").get()
}

/// Step once and run the TEAMS phase.
fn exchange(
    world: &mut World,
    room: &mut TeamShard,
    tick: u64,
    borrowed: &[BorderRecord<WirePos>],
    imports: &TeamImports,
) -> TeamExport {
    room.update(world, &ctx(tick));
    room.team_exchange(world, &ctx(tick), borrowed, imports)
        .expect("the composite takes part")
}

/// `team`'s exported wires, in export order.
fn exported(export: &TeamExport, team: u8) -> Vec<u64> {
    export
        .records
        .iter()
        .filter(|r| r.team == u64::from(team))
        .map(|r| r.wire)
        .collect()
}

/// `team`'s snapshot this tick, decoded: `(wire, x, y)` sorted.
fn view(world: &mut World, room: &mut TeamShard, tick: u64, team: u8) -> Vec<(u64, i32, i32)> {
    let mut out = BytesMut::new();
    room.snapshot(world, &ctx(tick), &Team(team), &[], &mut out);
    let snap = crate::testing::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    let mut v: Vec<(u64, i32, i32)> = snap.entities.iter().map(|r| (r.entity, r.x, r.y)).collect();
    v.sort_unstable();
    v
}

/// A fixture record body, as a remote shard's codec writes it.
fn body(wire: u64, x: i32, y: i32) -> Bytes {
    let mut buf = BytesMut::new();
    crate::testing::FixCodec.encode(wire, &WirePos { x, y }, &mut buf);
    buf.freeze()
}

fn imports(from: usize, tick: u64, records: &[(u8, u64, Bytes)]) -> TeamImports {
    let mut im = TeamImports::default();
    im.insert(TeamImport {
        from,
        tick,
        records: records
            .iter()
            .map(|(t, w, b)| TeamRecord {
                team: u64::from(*t),
                wire: *w,
                bytes: b.clone(),
            })
            .collect(),
    });
    im.settle();
    im
}

/// Per team: its members (players and wards) FIRST, then the other units
/// its units see — enemies and neutrals alike (a neutral elsewhere is
/// fog-gated). An unseen unit is not exported; the viewed teams are the
/// players' teams (a ward-only team is exported for, not viewed).
#[test]
fn a_shard_exports_each_teams_members_then_what_they_see() {
    let mut world = World::new();
    let mut room = shard0();
    let a = member(&mut world, &mut room, 1, 0, -80.0, -80.0);
    let b = member(&mut world, &mut room, 2, 1, -70.0, -80.0); // 10 from a
    let far = npc(&mut world, Some(1), -20.0, -20.0); // no one near
    let ward = npc(&mut world, Some(2), -90.0, -60.0); // 22.4 from a, 28.3 from b
    let seen_neutral = npc(&mut world, None, -85.0, -80.0); // 5 from a
    let lone_neutral = npc(&mut world, None, -20.0, -80.0);
    let export = exchange(&mut world, &mut room, 1, &[], &TeamImports::default());
    let (far, ward) = (wire_of(&world, far), wire_of(&world, ward));
    let (seen_neutral, lone) = (wire_of(&world, seen_neutral), wire_of(&world, lone_neutral));

    assert_eq!(export.views, [0, 1], "the players' teams");
    let mut t0 = exported(&export, 0);
    assert_eq!(t0.remove(0), a, "the member first");
    t0.sort_unstable();
    let mut want = vec![b, ward, seen_neutral];
    want.sort_unstable();
    assert_eq!(t0, want, "what a sees");
    let t1 = exported(&export, 1);
    assert_eq!(&t1[..2], &[b.min(far), b.max(far)], "members, wire order");
    assert!(t1.contains(&a) && t1.contains(&seen_neutral));
    assert!(!t1.contains(&ward), "the ward is 28.3 from b: unseen");
    let t2 = exported(&export, 2);
    assert_eq!(t2[0], ward, "a ward's team is exported for, member first");
    assert!(
        t2.contains(&a) && !t2.contains(&b),
        "a ward is a vision source"
    );
    assert!(
        export.records.iter().all(|r| r.wire != lone),
        "unseen: never"
    );
    assert!(
        view(&mut world, &mut room, 1, 0)
            .iter()
            .any(|r| r.0 == lone),
        "a neutral is shown to everyone on its own shard"
    );
}

/// An exported record follows its unit: the body is re-encoded when the
/// wire value changes (and reused while it does not).
#[test]
fn an_exported_record_follows_its_unit() {
    let mut world = World::new();
    let mut room = shard0();
    let a = member(&mut world, &mut room, 1, 0, -80.0, -80.0);
    let at = |export: &TeamExport| {
        let r = export
            .records
            .iter()
            .find(|r| r.wire == a)
            .expect("exported");
        let rec = crate::testing::Record::decode(r.bytes.as_ref()).expect("record");
        (rec.x, rec.y)
    };
    let first = exchange(&mut world, &mut room, 1, &[], &TeamImports::default());
    let again = exchange(&mut world, &mut room, 2, &[], &TeamImports::default());
    assert_eq!(at(&first), (-80, -80));
    assert_eq!(
        first.records[0].bytes.as_ptr(),
        again.records[0].bytes.as_ptr(),
        "an unchanged record reuses its encoded body"
    );
    let e = room.inner.wire_entity[&a];
    world.entity_mut(e).insert(Position { x: -70.0, y: -75.0 });
    let moved = exchange(&mut world, &mut room, 3, &[], &TeamImports::default());
    assert_eq!(at(&moved), (-70, -75));
}

/// The per-team budget keeps the members and cuts the tail of what they
/// see; the cut is counted (team 0: a + three seen, team 1: three
/// members + a — two cut each). The team's CONTENT is not cut.
#[test]
fn the_budget_keeps_members_and_cuts_the_seen_tail() {
    let mut world = World::new();
    let mut room = shard0().with_team_budget(2);
    let a = member(&mut world, &mut room, 1, 0, -80.0, -80.0);
    for i in 0..3 {
        npc(&mut world, Some(1), -75.0 + i as f32, -80.0);
    }
    let export = exchange(&mut world, &mut room, 1, &[], &TeamImports::default());
    let t0 = exported(&export, 0);
    assert_eq!(t0.len(), 2);
    assert_eq!(t0[0], a);
    assert_eq!(exported(&export, 1).len(), 2);
    assert_eq!(room.over_budget(), 4);
    assert_eq!(view(&mut world, &mut room, 1, 0).len(), 4, "content uncut");
}

/// One record per wire, own > lent > imported: an import naming an own
/// entity shows the OWN record (fresh position, not the import's bytes);
/// one naming a lent record shows the lent value; an import alone shows
/// its body verbatim. The import is what makes the first two VISIBLE
/// (no unit of team 0 here sees them).
#[test]
fn a_wire_shows_once_with_the_freshest_known_value() {
    let mut world = World::new();
    let mut room = shard0();
    let a = member(&mut world, &mut room, 1, 0, -80.0, -80.0);
    let own = npc(&mut world, Some(1), -20.0, -20.0);
    room.update(&mut world, &ctx(1));
    let own = wire_of(&world, own);
    let lent = BorderRecord {
        wire: SHARD_SERIAL_RANGE + 7,
        state: WirePos { x: 10, y: -10 },
    };
    let remote = 3 * SHARD_SERIAL_RANGE + 9;
    let im = imports(
        1,
        1,
        &[
            (0, own, body(own, 1, 1)),
            (0, lent.wire, body(lent.wire, 2, 2)),
            (0, remote, body(remote, 60, 60)),
        ],
    );
    let export = exchange(&mut world, &mut room, 2, std::slice::from_ref(&lent), &im);
    assert_eq!(
        view(&mut world, &mut room, 2, 0),
        [
            (a, -80, -80),
            (own, -20, -20),
            (lent.wire, 10, -10),
            (remote, 60, 60)
        ]
    );
    assert_eq!(exported(&export, 0), [a], "an import is never re-exported");
}
