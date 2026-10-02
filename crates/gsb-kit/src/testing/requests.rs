//! Requests and counters reach the game through every kit room: whether
//! a room answers game RPCs, or reports the game's own counters, does
//! not depend on its visibility strategy.

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use bytes::Bytes;
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::{LogicCounter, LogicCounters};
use gsb_core::room::{Action, GameLogic, TickCtx};
use gsb_core::rpc::{RequestDecision, RpcRequest};

use crate::aoi::AoiRoom;
use crate::common::InputSeq;
use crate::game::{Game, ShardGame, TeamGame};
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::sharded::{ShardedRoom, ShardedSpatialRoom, ShardedTeamRoom};
use crate::space::{ConvexSectors2, Grid2, GridPartition2, Sector, VisionGrid2};
use crate::team::{Team, TeamRoom};
use crate::testing::{Fixture, Position};

/// The request op the test game answers (any op: the room does not
/// look at it).
const OP: u16 = 4242;

/// A game that answers every request it is asked, and records it.
struct Recording {
    game: Fixture,
    asked: Vec<u16>,
}

impl Recording {
    fn new() -> Self {
        Self {
            game: Fixture::default(),
            asked: Vec::new(),
        }
    }
}

impl Game for Recording {
    type Codec = <Fixture as Game>::Codec;

    const SNAPSHOT_OP: u16 = <Fixture as Game>::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = <Fixture as Game>::PRIVATE_OP;

    fn codec(&self) -> &Self::Codec {
        self.game.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.game.spawn_player(world, conn)
    }
    fn spawn_player_as(&mut self, world: &mut World, conn: ConnectionId, identity: &str) -> Entity {
        self.game.spawn_player_as(world, conn, identity)
    }
    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        self.game.ingest(world, ctx, actions, players, seq);
    }
    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.game.systems(world, ctx);
    }
    fn handle_request(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        req: &RpcRequest,
        players: &HashMap<PlayerId, Entity>,
    ) -> Option<RequestDecision> {
        assert!(players.contains_key(&req.player), "the room's player table");
        self.asked.push(req.op);
        Some(RequestDecision::Reply(Bytes::from_static(b"answered")))
    }
    fn counters(&self, _world: &World, out: &mut LogicCounters) {
        out.put(&ASKED, 40 + self.asked.len() as u64);
    }
}

/// The test game's one counter.
const ASKED: LogicCounter = LogicCounter::sum("asked", "Requests the game was asked.");

impl TeamGame for Recording {
    fn team_of(&mut self, _world: &World, _conn: ConnectionId, _entity: Entity) -> Team {
        Team(0)
    }
}

impl ShardGame for Recording {
    type Mig = <Fixture as ShardGame>::Mig;

    fn capture(&self, world: &World, entity: Entity) -> Self::Mig {
        self.game.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: Self::Mig) -> Entity {
        self.game.restore(world, mig)
    }
}

/// Join one player on `room`, ask it a request, and check the game both
/// saw the request and produced the answer the room hands back.
fn assert_forwards<R: GameLogic<World>>(name: &str, mut room: R, game: impl Fn(&R) -> &Recording) {
    let mut world = World::new();
    let ctx = TickCtx {
        room: RoomId(1),
        tick: 1,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: Default::default(),
        paths: Default::default(),
    };
    let admission = room.on_join(&mut world, ConnectionId(1));
    let req = RpcRequest {
        conn: ConnectionId(1),
        player: admission.player,
        id: 7,
        op: OP,
        payload: Bytes::new(),
    };
    let decision = room.handle_request(&mut world, &ctx, &req);
    assert!(
        matches!(decision, Some(RequestDecision::Reply(ref body)) if &body[..] == b"answered"),
        "{name}: the room hands back the game's answer"
    );
    assert_eq!(
        game(&room).asked,
        [OP],
        "{name}: the request reached the game"
    );
}

/// Every kit room — open, AOI, team fog, PVS, sharded, sharded × spatial
/// — forwards a request to `Game::handle_request`.
#[test]
fn every_kit_room_forwards_requests_to_the_game() {
    assert_forwards("open", OpenRoom::with_game(Recording::new()), |r| r.game());
    assert_forwards(
        "aoi",
        AoiRoom::with_game(Recording::new(), Grid2::new(20.0)),
        |r| r.game(),
    );
    assert_forwards(
        "team",
        TeamRoom::with_game(Recording::new(), VisionGrid2::<Position>::new(25.0)),
        |r| r.game(),
    );
    let one_sector = ConvexSectors2::<Position>::new(
        vec![vec![
            (-50.0, -50.0),
            (50.0, -50.0),
            (50.0, 50.0),
            (-50.0, 50.0),
        ]],
        vec![vec![Sector(0)]],
    );
    assert_forwards(
        "pvs",
        SectorRoom::with_game(Recording::new(), one_sector),
        |r| r.game(),
    );
    let shard = || {
        ShardedRoom::with_game(
            Recording::new(),
            GridPartition2::<Position>::new(1, 50.0),
            0,
        )
    };
    assert_forwards("sharded", shard(), |r| r.game());
    assert_forwards(
        "sharded spatial",
        ShardedSpatialRoom::with_shard(shard(), Grid2::new(20.0)),
        |r| r.game(),
    );
}

/// `room` reports exactly the game's counter, as the game put it.
fn assert_counts<R: GameLogic<World>>(name: &str, room: R) {
    let mut out = LogicCounters::new();
    room.logic_counters(&World::new(), &mut out);
    assert_eq!(out.get("asked"), Some(40), "{name}: the game's counter");
    assert_eq!(out.slots().len(), 1, "{name}: and nothing of the room's");
}

/// Every kit room — open, AOI, team fog, PVS, sharded, sharded ×
/// spatial, sharded × team — forwards `Game::counters` (F9); a sharded
/// room without crystallization adds none of its own.
#[test]
fn every_kit_room_forwards_counters_to_the_game() {
    assert_counts("open", OpenRoom::with_game(Recording::new()));
    assert_counts(
        "aoi",
        AoiRoom::with_game(Recording::new(), Grid2::new(20.0)),
    );
    assert_counts(
        "team",
        TeamRoom::with_game(Recording::new(), VisionGrid2::<Position>::new(25.0)),
    );
    let one_sector = ConvexSectors2::<Position>::new(
        vec![vec![
            (-50.0, -50.0),
            (50.0, -50.0),
            (50.0, 50.0),
            (-50.0, 50.0),
        ]],
        vec![vec![Sector(0)]],
    );
    assert_counts("pvs", SectorRoom::with_game(Recording::new(), one_sector));
    let shard = || {
        ShardedRoom::with_game(
            Recording::new(),
            GridPartition2::<Position>::new(1, 50.0),
            0,
        )
    };
    assert_counts("sharded", shard());
    assert_counts(
        "sharded spatial",
        ShardedSpatialRoom::with_shard(shard(), Grid2::new(20.0)),
    );
    assert_counts(
        "sharded team",
        ShardedTeamRoom::with_shard(shard(), VisionGrid2::<Position>::new(25.0), |_| None),
    );
}
