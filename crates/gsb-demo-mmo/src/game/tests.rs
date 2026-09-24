//! The MMO's hooks at the logic level: spawning, the camps and the mob
//! lifecycle, the input path, the logout bot, capture/restore.

use std::time::Duration;

use gsb_core::id::RoomId;
use gsb_kit::identity::WireId;

use super::*;
use crate::components::Mob;
use crate::mmo::{Attack, Travel};
use crate::realm::MobSpawn;
use crate::world::{SHARDS, WORLD_HALF, home_shard};

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

fn mobs(world: &mut World) -> Vec<(Pos3, Vitals, Mob)> {
    let mut q = world.query::<(&Pos3, &Vitals, &Mob)>();
    q.iter(world).map(|(p, v, m)| (*p, *v, m.clone())).collect()
}

/// A login appears at its saved character's position; a session without
/// one at the shard's waystone.
#[test]
fn characters_spawn_at_their_saved_position() {
    let realm = Realm::empty().with_login(5, Pos3::new(10.0, 0.0, -20.0));
    let mut world = World::new();
    let mut game = MmoGame::for_shard(1, &realm);
    let saved = game.spawn_player(&mut world, ConnectionId(5));
    let fresh = game.spawn_player(&mut world, ConnectionId(6));
    assert_eq!(world.get::<Pos3>(saved), Some(&Pos3::new(10.0, 0.0, -20.0)));
    assert_eq!(
        world.get::<Pos3>(fresh),
        Some(&Pos3::new(256.0, 0.0, -256.0))
    );
    let v = world.get::<Vitals>(saved).expect("vitals");
    assert_eq!((v.kind, v.hp), (Kind::Player, PLAYER_HP));
}

/// A shard spawns only its own camps; a camp respawns on its period;
/// every mob walks its route at its own pace and despawns on schedule.
#[test]
fn camps_spawn_walk_and_despawn_mobs_on_their_own_ground() {
    let east = MobSpawn::once(Kind::Mob, Pos3::new(100.0, 0.0, -100.0), 2, 20, 50).walking(
        vec![[103.0, -100.0]],
        9.0,
        false,
    );
    let west = MobSpawn::once(Kind::Flyer, Pos3::new(-100.0, 90.0, -100.0), 2, 5, 50);
    let realm = Realm::empty()
        .with_spawn(MobSpawn {
            every: Some(10),
            ..east
        })
        .with_spawn(west);
    let mut world = World::new();
    let mut game = MmoGame::for_shard(1, &realm);
    for tick in 1..=2 {
        game.systems(&mut world, &ctx(tick));
    }
    let m = mobs(&mut world);
    assert_eq!(m.len(), 1, "only shard 1's camp spawns here: {m:?}");
    assert_eq!(m[0].2.dies_at, 22);
    for tick in 3..=12 {
        game.systems(&mut world, &ctx(tick));
    }
    let m = mobs(&mut world);
    assert_eq!(m.len(), 2, "the camp respawned at tick 12");
    assert!(
        m.iter()
            .any(|(p, _, _)| *p == Pos3::new(103.0, 0.0, -100.0))
    );
    for tick in 13..=22 {
        game.systems(&mut world, &ctx(tick));
    }
    assert_eq!(
        mobs(&mut world).len(),
        2,
        "22: the first died, the third came"
    );
    assert!(mobs(&mut world).iter().all(|(_, _, m)| m.dies_at > 22));
}

/// `Attack` hits a mob of this shard in reach and kills it at zero;
/// `Travel` teleports and cancels the walk; one sequence space. Driven
/// through the MMO's real kit room (the kit stamps the mob's wire id).
#[test]
fn attack_and_travel_go_through_the_input_path() {
    use gsb_core::room::GameLogic;
    let realm = Realm::empty()
        .with_login(1, Pos3::new(-240.0, 0.0, -256.0))
        .with_spawn(MobSpawn::once(
            Kind::Mob,
            Pos3::new(-236.0, 0.0, -256.0),
            1,
            99,
            30,
        ));
    let mut room = crate::mmo_shard(0, &realm);
    let mut world = World::new();
    let player = room.on_join(&mut world, ConnectionId(1)).player;
    room.update(&mut world, &ctx(1)); // the camp spawns, the kit stamps
    let mut q = world.query_filtered::<(Entity, &WireId), bevy_ecs::prelude::With<Mob>>();
    let (victim, wire) = q
        .iter(&world)
        .map(|(e, w)| (e, w.get()))
        .next()
        .expect("a mob");
    let act = |op, payload: Vec<u8>| Action {
        conn: ConnectionId(1),
        player,
        op,
        payload: payload.into(),
    };
    let hit = |seq| act(op::MMO_ATTACK, Attack { target: wire, seq }.encode_to_vec());
    room.ingest(&mut world, &ctx(2), &mut vec![hit(1), hit(1)]); // duplicate dropped
    assert_eq!(world.get::<Vitals>(victim).map(|v| v.hp), Some(5));
    room.ingest(&mut world, &ctx(3), &mut vec![hit(2)]);
    assert!(world.get_entity(victim).is_err(), "killed: despawned");

    let travel = Travel {
        waystone: 3,
        seq: 3,
    }
    .encode_to_vec();
    room.ingest(&mut world, &ctx(4), &mut vec![act(op::MMO_TRAVEL, travel)]);
    let mut q = world.query::<(&Pos3, Option<&MoveTarget>, &Vitals)>();
    let (at, walk, _) = q
        .iter(&world)
        .find(|(_, _, v)| v.kind == Kind::Player)
        .expect("hero");
    assert_eq!(
        (*at, walk),
        (Pos3::new(256.0, 0.0, 256.0), None),
        "teleported, walk cancelled"
    );
}

fn mobs_entity(world: &mut World) -> Entity {
    let mut q = world.query_filtered::<Entity, bevy_ecs::prelude::With<Mob>>();
    q.iter(world).next().expect("a mob")
}

/// The logout bot walks a bot-fed character to the nearest waystone
/// through the ordinary input path, then stays quiet.
#[test]
fn the_logout_bot_walks_to_the_nearest_waystone() {
    let mut world = World::new();
    let mut game = MmoGame::for_shard(3, &Realm::empty());
    let hero = game.spawn_player(&mut world, ConnectionId(1));
    world.entity_mut(hero).insert(Pos3::new(-40.0, 0.0, 300.0));
    let player = PlayerId(4);
    let mut actions = Vec::new();
    game.bot_actions(&world, &ctx(1), [(player, hero)].into_iter(), &mut actions);
    assert_eq!(actions.len(), 1);
    let players = HashMap::from([(player, hero)]);
    game.ingest(
        &mut world,
        &ctx(1),
        &mut actions,
        &players,
        &mut InputSeq::default(),
    );
    assert_eq!(
        world.get::<MoveTarget>(hero),
        Some(&MoveTarget {
            x: -256.0,
            z: 256.0
        })
    );
    let mut again = Vec::new();
    game.bot_actions(&world, &ctx(2), [(player, hero)].into_iter(), &mut again);
    assert!(again.is_empty(), "already walking there");
}

/// Capture/restore round-trips a player's walk and a mob's whole brain.
#[test]
fn capture_and_restore_carry_players_and_mobs() {
    let mut a = World::new();
    let mut b = World::new();
    let mut game = MmoGame::for_shard(0, &Realm::empty());
    let hero = game.spawn_player(&mut a, ConnectionId(1));
    a.entity_mut(hero).insert(MoveTarget { x: 5.0, z: -5.0 });
    let flyer = MobSpawn::once(Kind::Flyer, Pos3::new(-5.0, 80.0, -5.0), 1, 50, 40).walking(
        vec![[5.0, -5.0], [5.0, 5.0]],
        4.0,
        true,
    );
    crate::systems::Camps::new([flyer].into_iter()).run(&mut a, 7);
    let mob = mobs_entity(&mut a);
    for e in [hero, mob] {
        let mig = game.capture(&a, e);
        let back = game.restore(&mut b, mig.clone());
        assert_eq!(game.capture(&b, back), mig, "round trip");
    }
    let m = mobs(&mut b);
    assert_eq!(
        (m[0].0.y, m[0].1.kind, m[0].2.dies_at),
        (80.0, Kind::Flyer, 57)
    );
    let mut q = b.query::<&MoveTarget>();
    assert_eq!(q.iter(&b).count(), 1, "the player's walk travelled");
}

/// The live spawn table stays on the map, every shard runs camps, and
/// the flyer's circuit crosses all four regions (mobs DO migrate).
#[test]
fn the_standard_realm_covers_every_shard_and_crosses_seams() {
    let realm = Realm::standard();
    for s in &realm.spawns {
        assert!(
            s.at.x.abs() <= WORLD_HALF && s.at.z.abs() <= WORLD_HALF,
            "{s:?}"
        );
    }
    for shard in 0..SHARDS {
        assert!(realm.spawns_of(shard).count() >= 2, "shard {shard}");
    }
    let flyer = realm
        .spawns
        .iter()
        .find(|s| s.kind == Kind::Flyer)
        .expect("a flyer");
    let mut regions: Vec<usize> = flyer
        .route
        .iter()
        .map(|&[x, z]| home_shard(&Pos3::new(x, 0.0, z)))
        .collect();
    regions.sort_unstable();
    assert_eq!(regions, [0, 1, 2, 3]);
}
