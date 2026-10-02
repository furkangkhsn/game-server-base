//! The kit's kick verb ([`crate::game::kick`], E8) through every kit
//! room: a game kicks by ENTITY from its systems, and the room hands the
//! owning player to the core's verb (the tick context's kick queue)
//! before its `update` returns. An entity no player owns is ignored; a
//! game that never kicks leaves the world without the verb's resource.

use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::room::{GameLogic, Kick, KickQueue, TickCtx};

use crate::aoi::AoiRoom;
use crate::identity::WireId;
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::sharded::{ShardedRoom, ShardedSpatialRoom, ShardedTeamRoom};
use crate::space::{ConvexSectors2, Grid2, GridPartition2, VisionGrid2};
use crate::team::TeamRoom;
use crate::testing::{Fixture, KickMe, Kicker, Position, fix_lent_pos, fixture_map};

use super::super::kick::KitKicks;

type Game = Kicker<Fixture>;

fn game() -> Game {
    Kicker(Fixture::default())
}

/// The entity carrying wire id `wire`.
fn entity_of(world: &mut World, wire: u64) -> Entity {
    world
        .query::<(Entity, &WireId)>()
        .iter(world)
        .find(|(_, w)| w.get() == wire)
        .map(|(e, _)| e)
        .expect("a stamped entity")
}

/// Join two players through `room`, mark the first (and an NPC) for a
/// kick, run one `update` against a context lending a kick queue, and
/// return what reached the queue with the first player's id. A second
/// `update` with nothing marked must forward nothing more.
fn kicked<L: GameLogic<World>>(mut room: L) -> (Vec<Kick>, PlayerId) {
    let mut world = World::new();
    let first = room.on_join(&mut world, ConnectionId(1));
    let _second = room.on_join(&mut world, ConnectionId(2));
    let entity = entity_of(&mut world, first.entity);
    world.entity_mut(entity).insert(KickMe("cheating"));
    world.spawn(KickMe("an npc"));
    let queue = KickQueue::default();
    let ctx = TickCtx {
        room: RoomId(1),
        tick: 1,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: queue.kicks(),
        paths: Default::default(),
    };
    room.update(&mut world, &ctx);
    let got = queue.take();
    room.update(&mut world, &ctx);
    assert!(queue.take().is_empty(), "forwarded once");
    (got, first.player)
}

fn assert_kicked<L: GameLogic<World>>(room: L, name: &str) {
    let (got, player) = kicked(room);
    assert_eq!(
        got,
        vec![Kick {
            player,
            reason: "cheating".into()
        }],
        "{name}: the owner, once; the npc ignored"
    );
}

#[test]
fn every_kit_room_forwards_a_kick_by_entity_to_its_owner() {
    assert_kicked(OpenRoom::with_game(game()), "open");
    assert_kicked(
        AoiRoom::<Game, Grid2>::with_game(game(), Grid2::new(20.0)),
        "aoi",
    );
    assert_kicked(
        SectorRoom::<Game, ConvexSectors2<Position>>::with_game(game(), fixture_map()),
        "pvs",
    );
    assert_kicked(
        TeamRoom::with_game(game(), VisionGrid2::<Position>::new(25.0)),
        "team",
    );
    let sharded = || ShardedRoom::with_game(game(), GridPartition2::<Position>::new(4, 100.0), 0);
    assert_kicked(sharded(), "sharded");
    assert_kicked(
        ShardedSpatialRoom::with_shard(sharded(), Grid2::new(20.0)),
        "sharded × spatial",
    );
    assert_kicked(
        ShardedTeamRoom::with_shard(sharded(), VisionGrid2::new(25.0), fix_lent_pos),
        "sharded × team",
    );
}

/// A game that never kicks: the room's forward finds no resource, and
/// the world never gets one.
#[test]
fn a_game_that_never_kicks_leaves_the_world_as_it_was() {
    let mut room = OpenRoom::<Fixture>::new();
    let mut world = World::new();
    let _ = room.on_join(&mut world, ConnectionId(1));
    let queue = KickQueue::default();
    let ctx = TickCtx {
        room: RoomId(1),
        tick: 1,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: queue.kicks(),
        paths: Default::default(),
    };
    room.update(&mut world, &ctx);
    assert!(queue.take().is_empty());
    assert!(world.get_resource::<KitKicks>().is_none());
}
