//! [`Game::spawn_player_verified`](crate::game::Game::spawn_player_verified)
//! through every kit room (B21): the verified claims the core hands
//! `on_join_verified` reach the game's spawn (the fixture marks them with
//! [`Vouched`]); an `on_join_as` join carries none.

use bevy_ecs::prelude::World;
use bytes::Bytes;
use gsb_core::auth::Joiner;
use gsb_core::id::ConnectionId;
use gsb_core::room::GameLogic;

use crate::aoi::AoiRoom;
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::sharded::{ShardedRoom, ShardedSpatialRoom, ShardedTeamRoom};
use crate::space::{ConvexSectors2, Grid2, GridPartition2, VisionGrid2};
use crate::team::TeamRoom;
use crate::testing::{Fixture, Login, Position, Vouched, fix_lent_pos};

/// Join `ann` with claims and `bob` by name only through `room`; the
/// claims the game was handed, and the logins.
fn vouched<L: GameLogic<World>>(mut room: L) -> (Vec<Bytes>, usize) {
    let mut world = World::new();
    let claims = Bytes::from_static(b"{\"class\":\"mage\"}");
    let ann = Joiner::new("ann").with_claims(Some(&claims));
    room.on_join_verified(&mut world, ConnectionId(1), &ann);
    room.on_join_as(&mut world, ConnectionId(2), "bob");
    let seen: Vec<Bytes> = world
        .query::<&Vouched>()
        .iter(&world)
        .map(|v| v.0.clone())
        .collect();
    let logins = world.query::<&Login>().iter(&world).count();
    (seen, logins)
}

fn sharded() -> ShardedRoom<Fixture, GridPartition2<Position>> {
    ShardedRoom::with_game(Fixture::default(), GridPartition2::new(1, 50.0), 0)
}

#[test]
fn every_kit_room_hands_the_game_the_verified_claims() {
    let want = (vec![Bytes::from_static(b"{\"class\":\"mage\"}")], 2);
    assert_eq!(vouched(OpenRoom::<Fixture>::new()), want, "open");
    assert_eq!(vouched(AoiRoom::<Fixture, Grid2>::new(20.0)), want, "aoi");
    assert_eq!(
        vouched(SectorRoom::<Fixture, ConvexSectors2<Position>>::new()),
        want,
        "pvs"
    );
    assert_eq!(vouched(sharded()), want, "sharded");
    assert_eq!(
        vouched(
            ShardedSpatialRoom::<Fixture, GridPartition2<Position>, Grid2>::new(0, 4, 100.0, 20.0)
        ),
        want,
        "sharded × spatial"
    );
    let team = TeamRoom::with_game(Fixture::default(), VisionGrid2::<Position>::new(10.0));
    assert_eq!(vouched(team), want, "team");
    let grid = ShardedTeamRoom::with_shard(sharded(), VisionGrid2::new(10.0), fix_lent_pos);
    assert_eq!(vouched(grid), want, "sharded team");
}
