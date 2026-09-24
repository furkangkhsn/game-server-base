//! The team decided AT the spawn (KIT-ARCHITECTURE §10, A1): a game
//! whose spawn point depends on the team (an arena's team bases)
//! overrides `TeamGame::spawn_team_player`, and the team room takes the
//! team from it — it does not ask `team_of` afterwards.

use std::collections::HashMap;

use bevy_ecs::prelude::Entity;
use gsb_core::room::Action;

use super::*;
use crate::common::InputSeq;
use crate::game::{Game, TeamGame};
use crate::space::VisionGrid2;
use crate::testing::Fixture;

/// The fixture game with two team bases: joins alternate between the
/// teams by join ORDER, and each unit spawns at its team's base.
#[derive(Default)]
struct Bases {
    fixture: Fixture,
    joined: u8,
}

/// Team `t`'s base.
fn base(t: u8) -> Position {
    Position {
        x: if t == 0 { -30.0 } else { 30.0 },
        y: 0.0,
    }
}

impl Game for Bases {
    type Codec = <Fixture as Game>::Codec;

    fn codec(&self) -> &Self::Codec {
        self.fixture.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.spawn_team_player(world, conn).0
    }
    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        self.fixture.ingest(world, ctx, actions, players, seq);
    }
    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.fixture.systems(world, ctx);
    }
}

impl TeamGame for Bases {
    fn spawn_team_player(&mut self, world: &mut World, _conn: ConnectionId) -> (Entity, Team) {
        let team = self.joined % 2;
        self.joined += 1;
        (
            world.spawn((base(team), Speed(DEFAULT_SPEED))).id(),
            Team(team),
        )
    }
    fn team_of(&mut self, _world: &World, _conn: ConnectionId, _entity: Entity) -> Team {
        panic!("the team room asked team_of although spawn_team_player chose the team")
    }
}

/// Three joins on EVEN transport ids (the fixture's parity rule would put
/// them all in team 0) land in teams 0, 1, 0 — each at its team's base,
/// each recorded as its entity's `TeamMember` and grouped by it.
#[test]
fn the_team_room_takes_the_team_from_the_spawn() {
    let mut world = World::new();
    let mut room =
        super::super::TeamRoom::with_game(Bases::default(), VisionGrid2::<Position>::new(10.0));
    for (i, conn) in [2u64, 4, 6].into_iter().enumerate() {
        let admission = room.on_join(&mut world, ConnectionId(conn));
        let entity = room.player_entity[&admission.player];
        let want = Team((i % 2) as u8);
        assert_eq!(world.get::<TeamMember>(entity), Some(&TeamMember(want)));
        assert_eq!(world.get::<Position>(entity), Some(&base(want.0)));
        assert_eq!(room.group_of(&world, admission.player), want);
    }
}

/// A game that does NOT override it keeps the old two-step answer:
/// `spawn_player`, then `team_of` (the fixture: conn parity).
#[test]
fn the_default_spawns_then_asks_team_of() {
    let mut world = World::new();
    let mut room =
        super::super::TeamRoom::with_game(Fixture::default(), VisionGrid2::<Position>::new(10.0));
    for conn in [1u64, 2, 3] {
        let admission = room.on_join(&mut world, ConnectionId(conn));
        let entity = room.player_entity[&admission.player];
        let want = TeamMember(Team((conn % 2) as u8));
        assert_eq!(world.get::<TeamMember>(entity), Some(&want));
    }
}
