//! Per-unit sight in the team room (BACKLOG A8): a source carrying a
//! [`SightRadius`] sees by it — past the room radius and past the 3×3
//! block, or short of the room radius (a ward) — while the team's other
//! sources and the other teams keep the room radius.

use bevy_ecs::prelude::Entity;

use super::*;

/// The entity a join made (the tests write components on it).
fn entity_of(room: &TeamRoom, player: PlayerId) -> Entity {
    room.player_entity[&player]
}

/// The ids in `team`'s snapshot this tick.
fn package(world: &mut World, room: &mut TeamRoom, tick: u64, team: u8) -> BTreeSet<u64> {
    let mut out = bytes::BytesMut::new();
    room.snapshot(world, &ctx(tick), &Team(team), &[], &mut out);
    snap_ids(&out)
}

/// Room radius 25 (cells of 25). Team 0: a hero at (0, 0) with sight
/// 60, a ward at (200, 0) with sight 10, a plain unit at (400, 0).
/// Team 1: `far` 55 east of the hero (two cells away — outside the 3×3
/// block), `edge` exactly 60 north of it, `past` 60.5 south; `near` 20
/// from the ward (the room radius would see it, the ward does not);
/// `plain` 20 from the plain unit and `wide` 30 from it (the team's
/// widened test never lends the hero's radius to the plain unit).
#[test]
fn a_units_own_radius_sees_past_the_room_radius_and_a_ward_short_of_it() {
    let mut world = World::new();
    let mut room = TeamRoom::new(25.0);
    let (hero, hero_p) = place(&mut world, &mut room, ConnectionId(2), 0.0, 0.0);
    let (ward, ward_p) = place(&mut world, &mut room, ConnectionId(4), 200.0, 0.0);
    let (unit, _) = place(&mut world, &mut room, ConnectionId(6), 400.0, 0.0);
    let (far, _) = place(&mut world, &mut room, ConnectionId(1), 55.0, 0.0);
    let (edge, _) = place(&mut world, &mut room, ConnectionId(3), 0.0, 60.0);
    let (past, _) = place(&mut world, &mut room, ConnectionId(5), 0.0, -60.5);
    let (near, _) = place(&mut world, &mut room, ConnectionId(7), 220.0, 0.0);
    let (plain, _) = place(&mut world, &mut room, ConnectionId(9), 420.0, 0.0);
    let (wide, _) = place(&mut world, &mut room, ConnectionId(11), 400.0, 30.0);
    let hero_e = entity_of(&room, hero_p);
    world.entity_mut(hero_e).insert(SightRadius(60.0));
    let ward_e = entity_of(&room, ward_p);
    world.entity_mut(ward_e).insert(SightRadius(10.0));
    room.update(&mut world, &ctx(1));

    let own = [hero, ward, unit];
    let team0 = package(&mut world, &mut room, 1, 0);
    assert_eq!(
        team0,
        BTreeSet::from_iter(own.into_iter().chain([far, edge, plain]))
    );
    assert!(!team0.contains(&past) && !team0.contains(&near) && !team0.contains(&wide));

    // Team 1 keeps the room radius: `near` sees the ward (20), `plain`
    // the unit (20); nobody of team 1 is within 25 of the hero.
    let team1 = package(&mut world, &mut room, 1, 1);
    let enemies = [far, edge, past, near, plain, wide];
    assert_eq!(
        team1,
        BTreeSet::from_iter(enemies.into_iter().chain([ward, unit]))
    );

    // Without the component the hero is back on the room radius.
    world.entity_mut(hero_e).remove::<SightRadius>();
    room.update(&mut world, &ctx(2));
    let team0 = package(&mut world, &mut room, 2, 0);
    assert!(!team0.contains(&far) && !team0.contains(&edge));
}

/// A radius equal to the room radius on every unit changes no record
/// of any team's snapshot (the widened path agrees with the default
/// one; the records compare decoded — two rooms' maps iterate in their
/// own orders).
#[test]
fn the_room_radius_as_every_units_own_changes_no_record() {
    let layout = [
        (2, 0.0, 0.0),
        (1, 24.0, 7.0),
        (4, 60.0, 60.0),
        (3, 80.0, 60.0),
        (5, -30.0, 5.0),
        (6, 110.0, -40.0),
    ];
    let frames = |own: bool| {
        let mut world = World::new();
        let mut room = TeamRoom::new(25.0);
        for (conn, x, y) in layout {
            let (_, player) = place(&mut world, &mut room, ConnectionId(conn), x, y);
            if own {
                let e = entity_of(&room, player);
                world.entity_mut(e).insert(SightRadius(25.0));
            }
        }
        room.update(&mut world, &ctx(1));
        let mut out = Vec::new();
        for team in [0, 1] {
            let mut buf = bytes::BytesMut::new();
            room.snapshot(&mut world, &ctx(1), &Team(team), &[], &mut buf);
            let snap = crate::testing::WorldSnapshot::decode(buf.as_ref()).expect("snapshot");
            let mut records: Vec<(u64, i32, i32)> =
                snap.entities.iter().map(|r| (r.entity, r.x, r.y)).collect();
            records.sort_unstable();
            out.push(records);
        }
        out
    };
    let plain = frames(false);
    assert_eq!(plain[0].len(), 5, "team 0 sees two of three enemies");
    assert_eq!(plain, frames(true));
}
