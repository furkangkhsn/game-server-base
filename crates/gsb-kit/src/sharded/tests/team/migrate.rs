//! The team across a migration: the crossing carries it, the arrival
//! gets it back, and an arriving player's session is owed a full view.

use super::*;
use crate::team::SightRadius;

/// A team-1 player crosses from shard 0 into shard 1's region: the
/// crossing reports its team, and on shard 1 the rebuilt entity is a
/// team-1 member again (exported under team 1 there).
#[test]
fn a_migrating_entity_carries_its_team() {
    let mut w0 = World::new();
    let mut s0 = shard0();
    let wire = member(&mut w0, &mut s0, 1, 1, -5.0, -50.0);
    let e = s0.inner.wire_entity[&wire];
    w0.entity_mut(e).insert(Position { x: 5.0, y: -50.0 });
    s0.update(&mut w0, &ctx(1));
    let moves = s0.collect_migrations(&mut w0, 1);
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].state.team, Some(Team(1)));

    let mut w1 = World::new();
    let mut s1 = TeamShard::new(1, 4, 100.0, R);
    let m = moves.into_iter().next().expect("one");
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    let arrived = s1.inner.wire_entity[&wire];
    assert_eq!(w1.get::<TeamMember>(arrived), Some(&TeamMember(Team(1))));
    let export = exchange(&mut w1, &mut s1, 2, &[], &TeamImports::default());
    assert_eq!(exported(&export, 1), [wire]);
    assert_eq!(export.views, [1], "the arrival views its team here");
}

/// Delta mode: a player arriving in an established team group holds no
/// baseline for this shard's view — its next private frame is the
/// one-shot FULL, which lists the resident too.
#[test]
fn an_arriving_player_gets_the_one_shot_full() {
    use crate::testing::private::Payload;

    let mut w1 = World::new();
    let mut s1 = TeamShard::new(1, 4, 100.0, R).with_delta();
    let resident = member(&mut w1, &mut s1, 2, 0, 50.0, -50.0);
    exchange(&mut w1, &mut s1, 1, &[], &TeamImports::default());
    let mut out = BytesMut::new();
    assert!(s1.snapshot(&mut w1, &ctx(1), &Team(0), &[], &mut out));

    let arrival = 7;
    let mig = TeamMig {
        kit: KitMig {
            game: FixMig {
                pos: Position { x: 5.0, y: -50.0 },
                speed: Some(DEFAULT_SPEED),
                target: None,
            },
            park: None,
            input: None,
            pin: None,
        },
        team: Some(Team(0)),
        sight: None,
    };
    s1.on_migrate_in(&mut w1, arrival, mig, Some(PlayerId(9)));
    exchange(&mut w1, &mut s1, 2, &[], &TeamImports::default());
    out.clear();
    assert!(s1.snapshot(&mut w1, &ctx(2), &Team(0), &[], &mut out));
    let delta = crate::testing::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
    assert!(delta.delta, "the established group ships a delta");

    let mut pbuf = BytesMut::new();
    assert!(s1.private(&mut w1, PlayerId(9), &Team(0), &[], &mut pbuf));
    let frame = crate::testing::Private::decode(pbuf.as_ref()).expect("private");
    let Some(Payload::Snapshot(full)) = frame.payload else {
        panic!("the one-shot full: {frame:?}");
    };
    assert!(!full.delta);
    let ids: BTreeSet<u64> = full.entities.iter().map(|r| r.entity).collect();
    assert_eq!(ids, BTreeSet::from([resident, arrival]));
}

/// Delta mode: a player that leaves for another shard and comes back
/// holds no baseline for this shard's view any more — the view moved on
/// without it — so it gets the one-shot full again.
#[test]
fn a_player_back_from_another_shard_gets_a_full_again() {
    use crate::testing::private::Payload;

    let mut world = World::new();
    let mut room = shard0().with_delta();
    let wire = member(&mut world, &mut room, 1, 0, -5.0, -50.0);
    let player = room.inner.entity_player[&room.inner.wire_entity[&wire]];
    let owed = |room: &mut TeamShard, world: &mut World| {
        let mut pbuf = BytesMut::new();
        room.private(world, player, &Team(0), &[], &mut pbuf);
        let frame = crate::testing::Private::decode(pbuf.as_ref()).expect("private");
        matches!(frame.payload, Some(Payload::Snapshot(_)))
    };
    exchange(&mut world, &mut room, 1, &[], &TeamImports::default());
    let mut out = BytesMut::new();
    room.snapshot(&mut world, &ctx(1), &Team(0), &[], &mut out);
    owed(&mut room, &mut world);
    exchange(&mut world, &mut room, 2, &[], &TeamImports::default());
    room.snapshot(&mut world, &ctx(2), &Team(0), &[], &mut out);
    assert!(!owed(&mut room, &mut world), "baselined");

    // Out to shard 1 …
    let e = room.inner.wire_entity[&wire];
    world.entity_mut(e).insert(Position { x: 5.0, y: -50.0 });
    exchange(&mut world, &mut room, 3, &[], &TeamImports::default());
    // (The core asks every group every step: the group stays established.)
    room.snapshot(&mut world, &ctx(3), &Team(0), &[], &mut out);
    let mut moves = room.collect_migrations(&mut world, 1);
    room.on_migrate_out(&mut world, wire);
    // … and straight back.
    let m = moves.pop().expect("crossed");
    room.on_migrate_in(&mut world, m.wire, m.state, m.player);
    exchange(&mut world, &mut room, 4, &[], &TeamImports::default());
    out.clear();
    room.snapshot(&mut world, &ctx(4), &Team(0), &[], &mut out);
    assert!(owed(&mut room, &mut world), "the one-shot full again");
}

/// A unit's own sight radius (A8) crosses with it: the crossing carries
/// it, the arrival gets the component back and sees by it on its new
/// shard (an enemy 40 away — past the room radius).
#[test]
fn a_migrating_unit_carries_its_sight_radius() {
    let mut w0 = World::new();
    let mut s0 = shard0();
    let wire = member(&mut w0, &mut s0, 1, 0, -5.0, -50.0);
    let e = s0.inner.wire_entity[&wire];
    w0.entity_mut(e)
        .insert((Position { x: 5.0, y: -50.0 }, SightRadius(45.0)));
    s0.update(&mut w0, &ctx(1));
    let moves = s0.collect_migrations(&mut w0, 1);
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].state.sight, Some(SightRadius(45.0)));

    let mut w1 = World::new();
    let mut s1 = TeamShard::new(1, 4, 100.0, R);
    let enemy = member(&mut w1, &mut s1, 2, 1, 45.0, -50.0);
    let m = moves.into_iter().next().expect("one");
    s1.on_migrate_in(&mut w1, m.wire, m.state, m.player);
    let arrived = s1.inner.wire_entity[&wire];
    assert_eq!(w1.get::<SightRadius>(arrived), Some(&SightRadius(45.0)));
    let export = exchange(&mut w1, &mut s1, 2, &[], &TeamImports::default());
    assert_eq!(exported(&export, 0), [wire, enemy]);
}
