//! [`Game::spawn_player_as`] through every kit room that spawns with the
//! game's own hook: the identity the core hands `on_join_as` reaches the
//! game (the fixture marks a named login with [`Login`]); a plain
//! `on_join` is an anonymous one.

use bevy_ecs::prelude::World;
use gsb_core::id::ConnectionId;
use gsb_core::room::{Admission, GameLogic};

use crate::aoi::AoiRoom;
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::sharded::{ShardedRoom, ShardedSpatialRoom};
use crate::space::{ConvexSectors2, Grid2, GridPartition2};
use crate::testing::{Fixture, Login, Position};

/// Join `ann` by name and an anonymous session through `room`, then read
/// back the identities the game was handed (one [`Login`] per named
/// spawn).
fn logins<L: GameLogic<World>>(mut room: L) -> Vec<String> {
    let mut world = World::new();
    let named: Admission = room.on_join_as(&mut world, ConnectionId(1), "ann");
    let anonymous = room.on_join(&mut world, ConnectionId(2));
    assert_ne!(named.entity, anonymous.entity, "two players");
    let mut seen: Vec<String> = world
        .query::<&Login>()
        .iter(&world)
        .map(|l| l.0.clone())
        .collect();
    seen.sort();
    seen
}

#[test]
fn every_game_spawning_room_hands_the_game_the_identity() {
    let ann = vec!["ann".to_string()];
    assert_eq!(logins(OpenRoom::<Fixture>::new()), ann, "open");
    assert_eq!(logins(AoiRoom::<Fixture, Grid2>::new(20.0)), ann, "aoi");
    assert_eq!(
        logins(SectorRoom::<Fixture, ConvexSectors2<Position>>::new()),
        ann,
        "pvs"
    );
    assert_eq!(
        logins(ShardedRoom::<Fixture, GridPartition2<Position>>::new(
            0, 4, 100.0
        )),
        ann,
        "sharded"
    );
    assert_eq!(
        logins(
            ShardedSpatialRoom::<Fixture, GridPartition2<Position>, Grid2>::new(0, 4, 100.0, 20.0)
        ),
        ann,
        "sharded × spatial"
    );
}

mod claims;
mod kick;
