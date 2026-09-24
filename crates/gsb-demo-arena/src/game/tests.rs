//! The arena's hooks at the logic level: team assignment, spawn bases,
//! the bot's retreat through the input path.

use std::collections::HashMap;
use std::time::Duration;

use gsb_core::id::RoomId;

use super::*;
use crate::VISION_RADIUS;

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

/// Round-robin by JOIN ORDER, whatever the transport ids: seven joins
/// over three teams land 0, 1, 2, 0, 1, 2, 0 (sizes 3/2/2), each unit at
/// its team's base, team-mates side by side.
#[test]
fn teams_are_assigned_round_robin_by_join_order() {
    let mut world = World::new();
    let mut game = ArenaGame::default();
    let conns = [8u64, 2, 4, 6, 10, 12, 14]; // all even: parity would say one team
    let mut teams = Vec::new();
    for (i, conn) in conns.into_iter().enumerate() {
        let (unit, team) = game.spawn_team_player(&mut world, ConnectionId(conn));
        teams.push(team.0);
        let at = *world.get::<Pos3>(unit).expect("spawned with a position");
        let base = game.base_of(team.0);
        let slot = (i / 3) as f32;
        assert_eq!(at, Pos3::new(base.x + 1.5 * slot, 0.0, base.z), "join {i}");
    }
    assert_eq!(teams, [0, 1, 2, 0, 1, 2, 0]);
}

/// The bases are out of each other's vision (a fresh spawn sees only its
/// own team), for the default three teams and for more.
#[test]
fn team_bases_are_outside_each_others_vision() {
    for teams in [3u8, 4, 5] {
        let game = ArenaGame::with_teams(teams);
        for a in 0..teams {
            for b in (a + 1)..teams {
                let (p, q) = (game.base_of(a), game.base_of(b));
                let d = ((p.x - q.x).powi(2) + (p.y - q.y).powi(2) + (p.z - q.z).powi(2)).sqrt();
                assert!(
                    d > VISION_RADIUS,
                    "{teams} teams: bases {a}/{b} {d} m apart"
                );
            }
        }
    }
}

/// A bot-fed unit is sent home through the ORDINARY input path: one
/// unnumbered `MoveTo` to its base, decoded by `ingest` into the unit's
/// target; once it is heading there the bot stays quiet.
#[test]
fn a_bot_fed_unit_retreats_to_its_base_through_the_input_path() {
    let mut world = World::new();
    let mut game = ArenaGame::default();
    game.spawn_team_player(&mut world, ConnectionId(1)); // team 0
    let (unit, team) = game.spawn_team_player(&mut world, ConnectionId(2)); // team 1
    world.entity_mut(unit).insert(TeamMember(team)); // the kit's record
    let away = Pos3::new(-40.0, 10.0, 40.0);
    world
        .entity_mut(unit)
        .insert((away, MoveTarget3(Pos3::new(-45.0, 12.0, 45.0))));
    let player = PlayerId(7);
    let players = HashMap::from([(player, unit)]);

    let mut actions = Vec::new();
    game.bot_actions(&world, &ctx(1), [(player, unit)].into_iter(), &mut actions);
    assert_eq!(actions.len(), 1, "one retreat order");
    assert_eq!(actions[0].op, op::ARENA_MOVE_TO);

    let mut seq = InputSeq::default();
    game.ingest(&mut world, &ctx(1), &mut actions, &players, &mut seq);
    let target = world.get::<MoveTarget3>(unit).expect("a target").0;
    assert_eq!(Cm3::from(target), Cm3::from(game.base_of(1)));

    let mut again = Vec::new();
    game.bot_actions(&world, &ctx(2), [(player, unit)].into_iter(), &mut again);
    assert!(again.is_empty(), "already retreating: {again:?}");
}
