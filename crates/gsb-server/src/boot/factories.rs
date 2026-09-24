//! One factory per room build the axes can resolve to. The only
//! place the server names a concrete game type.

use std::sync::Arc;

use bevy_ecs::world::World;

use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::RoomLogic;
use gsb_protocol::MessageTable;

/// The demo composition: base protocol + demo game messages.
pub fn build_table() -> Arc<MessageTable> {
    let mut table = gsb_protocol::base_table();
    gsb_demo::register(&mut table);
    Arc::new(table)
}

/// The open-visibility room factory: an empty bevy `World` + an
/// [`gsb_demo::room::OpenRoom`] over a spawn map of half-size
/// `spawn_half`. Group key is `()` (one group per room) — the OPEN
/// strategy: everyone sees everything, every connection receives the
/// whole world (the unrestricted baseline the restricted visibility
/// strategies are measured against).
///
/// `economy` is the in-process economy service (the RPC pattern's
/// external-I/O reference adapter, see `gsb_demo::economy`): ONE service
/// per server (a platform service, not a per-room one), shared by clone
/// with every room the factory builds.
pub(super) fn open_room_factory(
    spawn_half: f32,
    disconnect_grace: std::time::Duration,
    economy: gsb_demo::economy::EconomyService,
) -> RoomFactory<World, (), (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(
            gsb_demo::room::OpenRoom::with_spawn_half(spawn_half)
                .with_disconnect_grace(disconnect_grace)
                .with_economy(economy.clone()),
        ) as Box<dyn RoomLogic<World, GroupKey = (), Strip = ()>>,
    })
}

/// The AOI room factory: an empty bevy `World` + an
/// [`gsb_demo::aoi::AoiRoom`] with the given `cell_size` (world units per
/// cell edge). Group key is a spatial [`gsb_demo::aoi::Cell`] — the
/// spatial path: one snapshot per cell, shared by reference with the
/// cell's occupants. Note the `RoomFactory`'s group-key associated type
/// differs from `open_room_factory`'s (`Cell` vs `()`), so the strategies
/// cannot be stored in one value — `start_inner` picks the factory at the
/// config boundary. This is entirely on the game/server side; `gsb-core`
/// stays generic over the group key and is untouched.
pub(super) fn aoi_room_factory(
    cell_size: f32,
    spawn_half: f32,
    disconnect_grace: std::time::Duration,
) -> RoomFactory<World, gsb_demo::aoi::Cell, (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(
            gsb_demo::aoi::AoiRoom::with_spawn_half(cell_size, spawn_half)
                .with_disconnect_grace(disconnect_grace),
        ) as Box<dyn RoomLogic<World, GroupKey = gsb_demo::aoi::Cell, Strip = ()>>,
    })
}

/// The team-fog room factory: an empty bevy `World` + a
/// [`gsb_demo::team::TeamRoom`] with the given `vision_radius`. Group key
/// is [`gsb_demo::team::Team`] (2 groups).
pub(super) fn team_room_factory(
    vision_radius: f32,
    spawn_half: f32,
    disconnect_grace: std::time::Duration,
) -> RoomFactory<World, gsb_demo::team::Team, (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(
            gsb_demo::team::TeamRoom::with_spawn_half(vision_radius, spawn_half)
                .with_disconnect_grace(disconnect_grace),
        ) as Box<dyn RoomLogic<World, GroupKey = gsb_demo::team::Team, Strip = ()>>,
    })
}

/// The PVS room factory: an empty bevy `World` + a
/// [`gsb_demo::pvs::SectorRoom`] (the demo map is built into the room).
/// Group key is [`gsb_demo::pvs::Sector`].
pub(super) fn pvs_room_factory(
    spawn_half: f32,
    disconnect_grace: std::time::Duration,
) -> RoomFactory<World, gsb_demo::pvs::Sector, (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(
            gsb_demo::pvs::SectorRoom::with_spawn_half(spawn_half)
                .with_disconnect_grace(disconnect_grace),
        ) as Box<dyn RoomLogic<World, GroupKey = gsb_demo::pvs::Sector, Strip = ()>>,
    })
}

/// The sharded room factory: `shard_count` shards, each an empty bevy
/// [`World`] + a [`gsb_demo::sharded::ShardedRoom`] over the same map
/// (half-size `spawn_half`). Unlike the other factories (which build one
/// `RoomLogic` room), this returns [`BuiltRoom::Sharded`]: N shard worlds
/// and N `ShardLogic` instances (the grid topology is the room's business,
/// the registry just wires the channels).
///
/// `economy` is the same ONE service-per-server the demo rooms share
/// (cloned into every shard — the Faz 3 promotion gives the sharded path
/// the RPC machinery, so its `ECONOMY` requests delegate like any other
/// room's).
///
/// `home_shard` routes a join to the shard owning the joiner's *spawn*
/// position (the deterministic `spawn_pos` → the grid region of that
/// point). The router is pure and synchronous (no await) — the registry
/// never blocks on it.
pub(super) fn sharded_room_factory(
    spawn_half: f32,
    shard_count: usize,
    disconnect_grace: std::time::Duration,
    economy: gsb_demo::economy::EconomyService,
) -> RoomFactory<World, (), gsb_demo::sharded::ShardedRoomState, gsb_demo::sharded::StripPos> {
    Arc::new(move |_id, _config| {
        let shards: Vec<
            gsb_core::registry::Shard<
                World,
                (),
                gsb_demo::sharded::ShardedRoomState,
                gsb_demo::sharded::StripPos,
            >,
        > = (0..shard_count)
            .map(|i| {
                (
                    World::new(),
                    Box::new(
                        gsb_demo::sharded::ShardedRoom::new(i, shard_count, spawn_half)
                            .with_disconnect_grace(disconnect_grace)
                            .with_economy(economy.clone()),
                    )
                        as Box<
                            dyn gsb_core::shard::ShardLogic<
                                    World,
                                    GroupKey = (),
                                    State = gsb_demo::sharded::ShardedRoomState,
                                    Strip = gsb_demo::sharded::StripPos,
                                >,
                        >,
                )
            })
            .collect();
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |conn| {
                let (x, y) = gsb_demo::room::spawn_pos(conn, spawn_half);
                gsb_demo::sharded::shard_at(x, y, spawn_half, shard_count)
            }),
        }
    })
}

/// The sharded SPATIAL composite factory (ROADMAP Faz B): the same
/// N-shard grid as [`sharded_room_factory`], but every shard is a
/// [`gsb_demo::sharded::ShardedSpatialRoom`] — cell-grouped spatial AOI
/// broadcast per shard (cells of `cell_size` world units, the config's
/// `aoi_cell_size`), with the borrowed border strip integrated into the
/// per-cell delta ledger. Same `BuiltRoom::Sharded` wiring and
/// `home_shard` routing; only the shard logic differs.
pub(super) fn sharded_spatial_room_factory(
    spawn_half: f32,
    shard_count: usize,
    cell_size: f32,
    disconnect_grace: std::time::Duration,
    economy: gsb_demo::economy::EconomyService,
) -> RoomFactory<
    World,
    gsb_demo::aoi::Cell,
    gsb_demo::sharded::ShardedRoomState,
    gsb_demo::sharded::StripPos,
> {
    Arc::new(move |_id, _config| {
        let shards: Vec<
            gsb_core::registry::Shard<
                World,
                gsb_demo::aoi::Cell,
                gsb_demo::sharded::ShardedRoomState,
                gsb_demo::sharded::StripPos,
            >,
        > = (0..shard_count)
            .map(|i| {
                (
                    World::new(),
                    Box::new(
                        gsb_demo::sharded::ShardedSpatialRoom::new(
                            i,
                            shard_count,
                            spawn_half,
                            cell_size,
                        )
                        .with_disconnect_grace(disconnect_grace)
                        .with_economy(economy.clone()),
                    )
                        as Box<
                            dyn gsb_core::shard::ShardLogic<
                                    World,
                                    GroupKey = gsb_demo::aoi::Cell,
                                    State = gsb_demo::sharded::ShardedRoomState,
                                    Strip = gsb_demo::sharded::StripPos,
                                >,
                        >,
                )
            })
            .collect();
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |conn| {
                let (x, y) = gsb_demo::room::spawn_pos(conn, spawn_half);
                gsb_demo::sharded::shard_at(x, y, spawn_half, shard_count)
            }),
        }
    })
}
