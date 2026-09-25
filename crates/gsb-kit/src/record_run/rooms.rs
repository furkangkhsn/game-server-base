//! The rooms of each kind over the twin's game, in either framing
//! (`FixCodec` or `PackedCodec` for `C`), and the twin factories.

use std::sync::Arc;

use bevy_ecs::prelude::World;
use gsb_core::id::RoomId;
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::RoomLogic;
use gsb_core::shard::ShardLogic;

use super::game::{FixLike, Pair};
use super::{CELL, HALF, RADIUS, SHARDS};
use crate::aoi::AoiRoom;
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::sharded::{KitMig, ShardedRoom, ShardedSpatialRoom, ShardedTeamRoom, TeamMig};
use crate::space::{Cell, Grid2, GridPartition2, Sector, VisionGrid2};
use crate::team::{Team, TeamRoom};
use crate::testing::{FixMig, Position, WirePos, fix_lent_pos, fixture_map};

/// Both rooms of a single-actor kind: room 2 runs the record run.
pub(super) fn single<G: 'static, Sp: 'static>(
    build: fn(bool) -> Box<dyn RoomLogic<World, GroupKey = G, Strip = Sp>>,
) -> RoomFactory<World, G, (), Sp> {
    Arc::new(move |id, _| BuiltRoom::Single {
        world: World::new(),
        logic: build(id == RoomId(2)),
    })
}

/// The 2×2 shard grid with its diagonals (every shard a neighbour of
/// every other: a teleport is one migration hop).
pub(super) fn partition() -> GridPartition2<Position> {
    GridPartition2::new(SHARDS, HALF).with_diagonals()
}

/// Shard `index`'s NPC anchor: inside its region, 20 from both seams
/// (its NPCs stay its own and show in the neighbours' strips).
pub(super) fn anchor(index: usize) -> (f32, f32) {
    let x = if index.is_multiple_of(2) { -20.0 } else { 20.0 };
    let y = if index < 2 { -20.0 } else { 20.0 };
    (x, y)
}

pub(super) type Single<G> = Box<dyn RoomLogic<World, GroupKey = G, Strip = ()>>;
pub(super) type Shard<G, St> =
    Box<dyn ShardLogic<World, GroupKey = G, State = St, Strip = WirePos>>;

pub(super) fn open<C: FixLike>() -> Single<()> {
    Box::new(OpenRoom::with_game(Pair::<C>::at((0.0, 0.0))))
}

pub(super) fn aoi<C: FixLike>() -> Single<Cell> {
    Box::new(AoiRoom::with_game(
        Pair::<C>::at((0.0, 0.0)),
        Grid2::new(CELL),
    ))
}

pub(super) fn team<C: FixLike>(delta: bool) -> Single<Team> {
    let room = TeamRoom::with_game(
        Pair::<C>::at((0.0, 0.0)),
        VisionGrid2::<Position>::new(RADIUS),
    );
    Box::new(if delta { room.with_delta() } else { room })
}

/// The NPCs wander inside the west sector (every viewer of it or of
/// the north-west one sees the whole crowd).
pub(super) fn pvs<C: FixLike>() -> Single<Sector> {
    Box::new(SectorRoom::with_game(
        Pair::<C>::at((-25.0, -20.0)),
        fixture_map(),
    ))
}

pub(super) fn plain<C: FixLike>(i: usize) -> ShardedRoom<Pair<C>, GridPartition2<Position>> {
    ShardedRoom::with_game(Pair::<C>::at(anchor(i)), partition(), i)
}

pub(super) fn spatial<C: FixLike>(i: usize) -> Shard<Cell, KitMig<FixMig>> {
    Box::new(ShardedSpatialRoom::with_shard(
        plain::<C>(i),
        Grid2::new(CELL),
    ))
}

pub(super) fn team_shard<C: FixLike>(i: usize, delta: bool) -> Shard<Team, TeamMig<FixMig>> {
    let room = ShardedTeamRoom::with_shard(plain::<C>(i), VisionGrid2::new(RADIUS), fix_lent_pos);
    Box::new(if delta { room.with_delta() } else { room })
}
