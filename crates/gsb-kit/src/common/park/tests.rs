//! The disconnect-park policy through EVERY kit room
//! (KIT-ARCHITECTURE §10, F4): the hold's grace and its end are the
//! game's choice (`with_disconnect_policy`), the default is unchanged
//! (a timed hold toward the bot), and the combat veto of an untimed hold
//! reaches the game (`Game::may_release`).

use std::time::Duration;

use bevy_ecs::prelude::{Entity, With, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Detach, ExpireTo, GameLogic};

use crate::aoi::AoiRoom;
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::sharded::{ShardedRoom, ShardedSpatialRoom};
use crate::space::{ConvexSectors2, Grid2, GridPartition2, VisionGrid2};
use crate::team::TeamRoom;
use crate::testing::{Fixture, InCombat, Position, Vetoing, fixture_map};

type G = Vetoing<Fixture>;

fn game() -> G {
    Vetoing(Fixture::default())
}

/// The two policy builders every room has.
trait Policy: GameLogic<World> + Sized {
    fn grace(self, grace: Duration) -> Self;
    fn policy(self, grace: Option<Duration>, to: ExpireTo) -> Self;
}

macro_rules! policy {
    ($($room:ty),* $(,)?) => {$(
        impl Policy for $room {
            fn grace(self, grace: Duration) -> Self {
                self.with_disconnect_grace(grace)
            }
            fn policy(self, grace: Option<Duration>, to: ExpireTo) -> Self {
                self.with_disconnect_policy(grace, to)
            }
        }
    )*};
}

policy!(
    OpenRoom<G>,
    AoiRoom<G, Grid2>,
    TeamRoom<G, VisionGrid2<Position>>,
    SectorRoom<G, ConvexSectors2<Position>>,
    ShardedRoom<G, GridPartition2<Position>>,
    ShardedSpatialRoom<G, GridPartition2<Position>, Grid2>,
);

fn sharded() -> ShardedRoom<G, GridPartition2<Position>> {
    ShardedRoom::with_game(game(), GridPartition2::new(1, 50.0), 0)
}

/// One joined player's disconnect answer under the room `room`.
fn disconnect(mut room: impl GameLogic<World>) -> Detach {
    let mut world = World::new();
    let admission = room.on_join(&mut world, ConnectionId(1));
    room.on_disconnect(&mut world, admission.player, "ann")
}

fn check_policy<R: Policy>(name: &str, make: impl Fn() -> R) {
    let d = Duration::from_secs(5);
    let hold = |grace, to| Detach::Hold { grace, to };
    let cases = [
        (
            disconnect(make()),
            hold(Some(crate::DEFAULT_DISCONNECT_GRACE), ExpireTo::AiHandover),
            "the default is unchanged",
        ),
        (
            disconnect(make().grace(d)),
            hold(Some(d), ExpireTo::AiHandover),
            "the grace builder",
        ),
        (
            disconnect(make().policy(Some(d), ExpireTo::Despawn)),
            hold(Some(d), ExpireTo::Despawn),
            "a logout timer",
        ),
        (
            disconnect(make().policy(None, ExpireTo::Despawn)),
            hold(None, ExpireTo::Despawn),
            "combat-held, then released",
        ),
        (
            disconnect(make().policy(None, ExpireTo::AiHandover)),
            hold(None, ExpireTo::AiHandover),
            "combat-held, then the bot",
        ),
        (
            disconnect(make().policy(Some(Duration::ZERO), ExpireTo::AiHandover)),
            Detach::Despawn,
            "a zero grace parks nothing",
        ),
        (
            disconnect(make().policy(None, ExpireTo::Despawn).grace(d)),
            hold(Some(d), ExpireTo::Despawn),
            "the grace builder keeps the chosen end",
        ),
    ];
    for (got, want, what) in cases {
        assert_eq!(got, want, "{name}: {what}");
    }
}

/// Every room answers the configured grace and end.
#[test]
fn every_room_answers_the_configured_policy() {
    check_policy("open", || OpenRoom::with_game(game()));
    check_policy("aoi", || AoiRoom::with_game(game(), Grid2::new(20.0)));
    check_policy("team", || {
        TeamRoom::with_game(game(), VisionGrid2::new(10.0))
    });
    check_policy("pvs", || SectorRoom::with_game(game(), fixture_map()));
    check_policy("sharded", sharded);
    check_policy("sharded × spatial", || {
        ShardedSpatialRoom::with_shard(sharded(), Grid2::new(20.0))
    });
}

fn check_veto(name: &str, mut room: impl GameLogic<World>) {
    let mut world = World::new();
    let admission = room.on_join(&mut world, ConnectionId(1));
    let entity = world
        .query_filtered::<Entity, With<Position>>()
        .iter(&world)
        .next()
        .expect("the joined player's entity");
    world.entity_mut(entity).insert(InCombat);
    assert!(
        !room.may_release(&mut world, admission.player),
        "{name}: the game vetoes while in combat"
    );
    world.entity_mut(entity).remove::<InCombat>();
    assert!(
        room.may_release(&mut world, admission.player),
        "{name}: out of combat, released"
    );
    assert!(
        room.may_release(&mut world, PlayerId(999)),
        "{name}: no entity, nothing to hold"
    );
}

/// Every room hands the core's release question to the game, about the
/// parked player's entity.
#[test]
fn every_room_asks_the_game_before_ending_an_untimed_hold() {
    check_veto("open", OpenRoom::with_game(game()));
    check_veto("aoi", AoiRoom::with_game(game(), Grid2::new(20.0)));
    check_veto(
        "team",
        TeamRoom::with_game(game(), VisionGrid2::<Position>::new(10.0)),
    );
    check_veto("pvs", SectorRoom::with_game(game(), fixture_map()));
    check_veto("sharded", sharded());
    check_veto(
        "sharded × spatial",
        ShardedSpatialRoom::with_shard(sharded(), Grid2::new(20.0)),
    );
}
