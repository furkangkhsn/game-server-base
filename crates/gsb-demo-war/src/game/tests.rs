//! The hooks on a bare world: placement and faction rule, the welcome,
//! the map's static units, the capture rule, the walk, migration state
//! and the retreat bot. Room behaviour (fog, relay, combat across a
//! seam) is tested through the real actors in `tests/`.

use std::time::Duration;

use bevy_ecs::prelude::World;
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::room::TickCtx;
use gsb_kit::game::{Game, ShardGame, TeamGame};
use gsb_kit::team::{Team, TeamMember};
use prost::Message;

use super::*;
use crate::components::{Capture, Pos3};
use crate::realm::faction_of;
use crate::world::{BASES, CAPTURE_TICKS, POINTS, SHARDS, home_shard, tower};

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

fn units(world: &mut World, kind: Kind) -> Vec<(Pos3, Unit, Option<Team>)> {
    let mut q = world.query::<(&Pos3, &Unit, Option<&TeamMember>)>();
    q.iter(world)
        .filter(|(_, u, _)| u.kind == kind)
        .map(|(p, u, m)| (*p, *u, m.map(|m| m.0)))
        .collect()
}

/// A saved character appears where it was saved, on its side; an
/// unsaved one on its hashed faction's side, within 10 m of that
/// faction's base — the router's answer, from the same realm.
#[test]
fn a_login_appears_where_the_realm_places_it() {
    let saved = Pos3::ground(300.0, 250.0);
    let realm = Realm::empty().with_login("ann", Team(2), saved);
    let mut game = WarGame::for_shard(3, &realm);
    let mut world = World::new();
    let (e, team) = game.spawn_team_player_as(&mut world, ConnectionId(1), "ann");
    assert_eq!((world.get::<Pos3>(e), team), (Some(&saved), Team(2)));
    assert_eq!(world.get::<Unit>(e).map(|u| u.faction), Some(Some(Team(2))));
    assert_eq!(home_shard(&realm.placement("ann").at), 3);

    let (e, team) = game.spawn_team_player_as(&mut world, ConnectionId(2), "bob");
    assert_eq!(team, faction_of("bob"));
    let [bx, bz] = BASES[usize::from(team.0)];
    let at = *world.get::<Pos3>(e).expect("placed");
    assert!(at.ground_dist(&Pos3::ground(bx, bz)) <= 15.0, "{at:?}");
    assert_eq!(at, realm.placement("bob").at, "router and spawn agree");
    assert_eq!(
        home_shard(&at),
        usize::from(team.0),
        "a base is in its region"
    );
}

/// The faction rule is deterministic and deals every faction.
#[test]
fn unsaved_players_are_dealt_every_faction_deterministically() {
    let mut seen = [0u32; 3];
    for i in 0..300 {
        let name = format!("player-{i}");
        let t = faction_of(&name);
        assert_eq!(t, faction_of(&name));
        seen[usize::from(t.0)] += 1;
    }
    assert!(seen.iter().all(|&n| n > 60), "a fair spread: {seen:?}");
}

/// The welcome names the unit's faction, 1-based, and the faction count.
#[test]
fn the_welcome_names_the_faction() {
    let mut game = WarGame::for_shard(0, &Realm::empty());
    let mut world = World::new();
    let e = world.spawn(TeamMember(Team(1))).id();
    let mut out = bytes::BytesMut::new();
    assert!(game.session_private(&world, e, &mut out));
    let w = crate::war::Welcome::decode(&out[..]).expect("a welcome");
    assert_eq!((w.faction, w.factions), (2, 3));
    let bare = world.spawn(()).id();
    assert!(!game.session_private(&world, bare, &mut bytes::BytesMut::new()));
}

/// Each shard raises its region's three towers (one per faction, each
/// its faction's `TeamMember`) and the capture points on its ground —
/// once.
#[test]
fn every_region_has_a_tower_of_every_faction() {
    let mut points = 0;
    for index in 0..SHARDS {
        let mut game = WarGame::for_shard(index, &Realm::empty());
        let mut world = World::new();
        game.systems(&mut world, &ctx(1));
        game.systems(&mut world, &ctx(2));
        let towers = units(&mut world, Kind::Tower);
        assert_eq!(towers.len(), 3, "shard {index}");
        for (pos, unit, member) in towers {
            let f = member.expect("a tower is a faction's unit");
            assert_eq!(unit.faction, Some(f));
            assert_eq!(home_shard(&pos), index);
            assert_eq!([pos.x, pos.z], tower(f, index));
        }
        for (pos, unit, member) in units(&mut world, Kind::Point) {
            assert_eq!((unit.faction, member), (None, None), "neutral");
            assert_eq!(home_shard(&pos), index);
            points += 1;
        }
    }
    assert_eq!(points, POINTS.len());
}

fn point_world() -> (WarGame, World, bevy_ecs::prelude::Entity) {
    let [x, z] = POINTS[0];
    let mut game = WarGame::for_shard(home_shard(&Pos3::ground(x, z)), &Realm::empty());
    let mut world = World::new();
    game.systems(&mut world, &ctx(1));
    let mut q = world.query::<(bevy_ecs::prelude::Entity, &Capture, &Pos3)>();
    let point = q
        .iter(&world)
        .find(|(_, _, p)| [p.x, p.z] == [x, z])
        .map(|(e, _, _)| e)
        .expect("the middle point");
    (game, world, point)
}

fn player_at(world: &mut World, x: f32, z: f32, f: u8) -> bevy_ecs::prelude::Entity {
    let unit = Unit {
        kind: Kind::Player,
        faction: Some(Team(f)),
        hp: PLAYER_HP,
    };
    world
        .spawn((Pos3::ground(x, z), unit, TeamMember(Team(f))))
        .id()
}

/// A faction alone at a point for `CAPTURE_TICKS` in a row takes it: the
/// point becomes its unit (`Unit::faction` and `TeamMember`). A rival's
/// arrival resets the count; the owner alone does not count again.
#[test]
fn a_point_falls_to_the_faction_holding_it_alone() {
    let (mut game, mut world, point) = point_world();
    let [x, z] = POINTS[0];
    player_at(&mut world, x + 5.0, z, 1);
    for t in 2..(1 + CAPTURE_TICKS as u64) {
        game.systems(&mut world, &ctx(t));
    }
    assert_eq!(world.get::<TeamMember>(point), None, "one tick short");
    let rival = player_at(&mut world, x, z - 10.0, 2);
    game.systems(&mut world, &ctx(200));
    assert_eq!(world.get::<TeamMember>(point), None, "contested: not taken");
    assert_eq!(world.get::<Capture>(point), Some(&Capture { claim: None }));
    world.despawn(rival);
    for t in 0..CAPTURE_TICKS as u64 {
        game.systems(&mut world, &ctx(300 + t));
    }
    assert_eq!(world.get::<TeamMember>(point), Some(&TeamMember(Team(1))));
    assert_eq!(
        world.get::<Unit>(point).map(|u| u.faction),
        Some(Some(Team(1)))
    );
    game.systems(&mut world, &ctx(500));
    assert_eq!(world.get::<Capture>(point), Some(&Capture { claim: None }));
}

/// A player runs to its target at the run speed and stops on it.
#[test]
fn a_player_runs_to_its_target_and_stops() {
    let mut game = WarGame::for_shard(0, &Realm::empty());
    let mut world = World::new();
    let p = player_at(&mut world, -500.0, -500.0, 0);
    world.entity_mut(p).insert(MoveTarget {
        x: -500.0,
        z: -490.0,
    });
    game.systems(&mut world, &ctx(1));
    let at = *world.get::<Pos3>(p).expect("pos");
    assert!((at.z - (-500.0 + 7.0 / 30.0)).abs() < 1e-3, "{at:?}");
    for t in 2..60 {
        game.systems(&mut world, &ctx(t));
    }
    assert_eq!(world.get::<Pos3>(p), Some(&Pos3::ground(-500.0, -490.0)));
}

/// `capture` / `restore` carry the whole unit: position, unit, walk.
#[test]
fn migration_state_round_trips() {
    let game = WarGame::for_shard(0, &Realm::empty());
    let mut world = World::new();
    let p = player_at(&mut world, -1.0, 3.0, 2);
    world.entity_mut(p).insert(MoveTarget { x: 50.0, z: 3.0 });
    let mig = game.capture(&world, p);
    let mut other = WarGame::for_shard(1, &Realm::empty());
    let mut there = World::new();
    let q = other.restore(&mut there, mig.clone());
    assert_eq!(game.capture(&there, q), mig);
    assert_eq!(mig.target, Some(MoveTarget { x: 50.0, z: 3.0 }));
}

/// The retreat bot walks a unit home to its faction's base — once.
#[test]
fn the_retreat_bot_walks_home() {
    let mut game = WarGame::for_shard(3, &Realm::empty());
    let mut world = World::new();
    let p = player_at(&mut world, 100.0, 100.0, 1);
    let mut out = Vec::new();
    game.bot_actions(&world, &ctx(1), [(PlayerId(4), p)].into_iter(), &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].op, crate::op::WAR_MOVE_TO);
    let m = crate::war::MoveTo::decode(&out[0].payload[..]).expect("a move");
    assert_eq!((m.x, m.z, m.seq), (5_600, -5_600, 0));
    let [x, z] = BASES[1];
    world.entity_mut(p).insert(MoveTarget { x, z });
    out.clear();
    game.bot_actions(&world, &ctx(2), [(PlayerId(4), p)].into_iter(), &mut out);
    assert!(out.is_empty(), "already walking home");
}
