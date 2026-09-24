//! More than two teams (KIT-ARCHITECTURE §8.4): the team count is the
//! game's assignment policy, not a room constant — the 3D arena demo
//! runs more than two.

use std::collections::HashMap;

use bevy_ecs::prelude::Entity;
use gsb_core::room::Action;

use super::*;
use crate::kit::common::InputSeq;
use crate::kit::game::{Game, TeamGame};
use crate::kit::seam::DemoGame;
use crate::kit::space::VisionGrid2;

/// The demo game with a three-team assignment rule (conn mod 3).
struct ThreeTeams(DemoGame);

impl Game for ThreeTeams {
    type Codec = <DemoGame as Game>::Codec;

    fn codec(&self) -> &Self::Codec {
        self.0.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.0.spawn_player(world, conn)
    }
    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        self.0.ingest(world, ctx, actions, players, seq);
    }
    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.0.systems(world, ctx);
    }
}

impl TeamGame for ThreeTeams {
    fn team_of(&mut self, _world: &World, conn: ConnectionId, _entity: Entity) -> Team {
        Team((conn.0 % 3) as u8)
    }
}

/// Three teams, each its own group: own units at any range, enemies of
/// EITHER other team only when one of the team's units sees them, and a
/// neutral entity in all three packages.
#[test]
fn three_teams_each_see_own_units_and_enemies_in_their_vision() {
    let mut world = World::new();
    let mut room = super::super::TeamRoom::with_game(
        ThreeTeams(DemoGame::default()),
        VisionGrid2::<Position>::new(25.0),
    );
    let mut place = |world: &mut World, conn: u64, x: f32, y: f32| {
        let admission = room.on_join(world, ConnectionId(conn));
        let entity = room.player_entity[&admission.player];
        world.entity_mut(entity).insert(Position { x, y });
        (admission.entity, admission.player)
    };
    let (a, pa) = place(&mut world, 3, 0.0, 0.0); // team 0
    let (b, pb) = place(&mut world, 1, 10.0, 0.0); // team 1: 10 from A
    let (c, pc) = place(&mut world, 2, 40.0, 0.0); // team 2: 40 from A, 30 from B
    let (d, _) = place(&mut world, 5, 12.0, 0.0); // team 2: 12 from A, 2 from B
    let neutral = world
        .spawn((Position { x: 400.0, y: 400.0 }, Speed(DEFAULT_SPEED)))
        .id();
    room.update(&mut world, &ctx(1));
    let n = world
        .entity(neutral)
        .get::<crate::kit::identity::WireId>()
        .expect("stamped")
        .get();

    assert_eq!(room.group_of(&world, pa), Team(0));
    assert_eq!(room.group_of(&world, pb), Team(1));
    assert_eq!(room.group_of(&world, pc), Team(2));

    let mut package = |team: u8| {
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(team), &[], &mut out));
        snap_ids(&out)
    };
    assert_eq!(package(0), BTreeSet::from([a, b, d, n]), "team 0");
    assert_eq!(package(1), BTreeSet::from([a, b, d, n]), "team 1");
    assert_eq!(package(2), BTreeSet::from([a, b, c, d, n]), "team 2");
}
