//! The game's session payload (`Private.game`) through EVERY kit room:
//! owed once per session — the first private frame after a join or a
//! resume — riding the frame the room ships anyway (the delta rooms'
//! one-shot full included); and a game that keeps the default hook gets
//! exactly the frames it got before the hook existed.

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use bytes::BytesMut;
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::room::{Action, GameLogic, TickCtx};
use prost::Message;

use crate::aoi::AoiRoom;
use crate::common::InputSeq;
use crate::game::{Game, ShardGame, TeamGame};
use crate::identity::WireId;
use crate::proto::{Private, private::Payload};
use crate::pvs::SectorRoom;
use crate::room::OpenRoom;
use crate::sharded::{ShardedRoom, ShardedSpatialRoom};
use crate::space::{Grid2, GridPartition2, VisionGrid2};
use crate::team::{Team, TeamRoom};
use crate::testing::{FixCodec, FixMig, Fixture, Position, fixture_map};

mod dropped;

/// The fixture, telling each session `[0xA5, its wire id]` — or, with
/// `empty`, an empty payload (a proto3 message whose fields are all
/// zero) that must still be sent.
#[derive(Default)]
struct Greeting {
    inner: Fixture,
    empty: bool,
}

impl Game for Greeting {
    type Codec = <Fixture as Game>::Codec;
    const SNAPSHOT_OP: u16 = Fixture::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = Fixture::PRIVATE_OP;

    fn codec(&self) -> &Self::Codec {
        self.inner.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.inner.spawn_player(world, conn)
    }
    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        self.inner.ingest(world, ctx, actions, players, seq);
    }
    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.inner.systems(world, ctx);
    }
    fn session_private(&mut self, world: &World, entity: Entity, out: &mut BytesMut) -> bool {
        if !self.empty {
            let wire = world.get::<WireId>(entity).expect("a stamped player");
            out.extend_from_slice(&[0xA5, wire.get() as u8]);
        }
        true
    }
}

impl TeamGame for Greeting {
    fn team_of(&mut self, _: &World, _: ConnectionId, _: Entity) -> Team {
        Team(0) // one team: every joiner shares the group
    }
}

impl ShardGame for Greeting {
    type Mig = FixMig;
    fn capture(&self, world: &World, entity: Entity) -> FixMig {
        self.inner.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: FixMig) -> Entity {
        self.inner.restore(world, mig)
    }
}

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: Default::default(),
        paths: Default::default(),
    }
}

/// One tick of `players`' broadcast: the room's update, its groups'
/// snapshots, then each player's private frame (`None`: no frame).
fn tick<R: GameLogic<World>>(
    room: &mut R,
    world: &mut World,
    n: u64,
    players: &[PlayerId],
) -> Vec<Option<BytesMut>> {
    let groups: Vec<_> = players.iter().map(|&p| room.group_of(world, p)).collect();
    room.update(world, &ctx(n));
    let mut done = Vec::new();
    for g in &groups {
        if !done.contains(g) {
            let mut out = BytesMut::new();
            room.snapshot(world, &ctx(n), g, &[], &mut out);
            done.push(g.clone());
        }
    }
    players
        .iter()
        .zip(&groups)
        .map(|(&p, g)| {
            let mut out = BytesMut::new();
            room.private(world, p, g, &[], &mut out).then_some(out)
        })
        .collect()
}

/// A frame's `game` field (`None`: no frame, or no field 4 in it).
fn game_of(frame: &Option<BytesMut>) -> Option<Vec<u8>> {
    let frame = frame.as_ref()?;
    let private = Private::decode(frame.as_ref()).expect("a private frame");
    // proto3 `bytes`: an absent field and an empty one decode alike, so
    // look for the tag itself for the empty-payload case.
    let has_field_4 = frame.ends_with(&[0x22, 0x00]) || !private.game.is_empty();
    has_field_4.then_some(private.game)
}

fn is_one_shot_full(frame: &Option<BytesMut>) -> bool {
    frame.as_ref().is_some_and(|f| {
        matches!(
            Private::decode(f.as_ref())
                .expect("a private frame")
                .payload,
            Some(Payload::Snapshot(_))
        )
    })
}

/// Join `a`, tick; join `b` into the now-established group, tick twice;
/// then `b` drops and resumes, tick. What `a` and `b` were told.
fn check<R: GameLogic<World>>(name: &str, mut room: R, one_shot: bool) {
    let mut world = World::new();
    let a = room.on_join(&mut world, ConnectionId(1));
    let frames = tick(&mut room, &mut world, 1, &[a.player]);
    assert_eq!(
        game_of(&frames[0]),
        Some(vec![0xA5, a.entity as u8]),
        "{name}: the joiner's first frame carries its payload"
    );

    let b = room.on_join(&mut world, ConnectionId(3));
    let frames = tick(&mut room, &mut world, 2, &[a.player, b.player]);
    assert!(frames[0].is_none(), "{name}: once per session");
    assert_eq!(
        game_of(&frames[1]),
        Some(vec![0xA5, b.entity as u8]),
        "{name}: the second joiner's payload"
    );
    assert_eq!(
        is_one_shot_full(&frames[1]),
        one_shot,
        "{name}: the payload rides the one-shot full where there is one"
    );
    let frames = tick(&mut room, &mut world, 3, &[a.player, b.player]);
    assert!(frames.iter().all(Option::is_none), "{name}: nothing more");

    room.on_disconnect(&mut world, b.player, "bee");
    room.on_resume(&mut world, "bee", ConnectionId(4), b.player, b.entity);
    let frames = tick(&mut room, &mut world, 4, &[a.player, b.player]);
    assert!(
        frames[0].is_none(),
        "{name}: the other session is not re-told"
    );
    assert_eq!(
        game_of(&frames[1]),
        Some(vec![0xA5, b.entity as u8]),
        "{name}: a resumed session is told again"
    );
}

fn sharded<G: ShardGame<Codec = FixCodec>>(game: G) -> ShardedRoom<G, GridPartition2<Position>> {
    ShardedRoom::with_game(game, GridPartition2::new(1, 50.0), 0)
}

/// Every room sends the session payload once per session.
#[test]
fn every_room_tells_each_session_once() {
    let g = Greeting::default;
    check("open", OpenRoom::with_game(g()), false);
    check("aoi", AoiRoom::with_game(g(), Grid2::new(20.0)), true);
    check(
        "team",
        TeamRoom::with_game(g(), VisionGrid2::<Position>::new(10.0)),
        false,
    );
    check(
        "team (delta)",
        TeamRoom::with_game(g(), VisionGrid2::<Position>::new(10.0)).with_delta(),
        true,
    );
    check("pvs", SectorRoom::with_game(g(), fixture_map()), false);
    check("sharded", sharded(g()), false);
    check(
        "sharded × spatial",
        ShardedSpatialRoom::with_shard(sharded(g()), Grid2::new(20.0)),
        true,
    );
}

/// An empty payload is still sent: the game said it has one.
#[test]
fn an_empty_payload_is_sent() {
    let mut room = OpenRoom::with_game(Greeting {
        empty: true,
        ..Greeting::default()
    });
    let mut world = World::new();
    let a = room.on_join(&mut world, ConnectionId(1));
    let frames = tick(&mut room, &mut world, 1, &[a.player]);
    assert_eq!(frames[0].as_deref(), Some(&[0x22, 0x00][..]));
}

/// The default hook: a join's first tick ships no private frame where
/// none was shipped before, and the AOI rooms' one-shot full carries no
/// field 4 (it re-encodes, through the generated type, to the same
/// bytes — `game` empty is omitted).
fn check_default<R: GameLogic<World>>(name: &str, mut room: R, one_shot: bool) {
    let mut world = World::new();
    let a = room.on_join(&mut world, ConnectionId(1));
    let frames = tick(&mut room, &mut world, 1, &[a.player]);
    assert!(frames[0].is_none(), "{name}: no frame for the first joiner");
    let b = room.on_join(&mut world, ConnectionId(3));
    let frames = tick(&mut room, &mut world, 2, &[a.player, b.player]);
    assert!(frames[0].is_none(), "{name}");
    assert_eq!(
        frames[1].is_some(),
        one_shot,
        "{name}: only the one-shot full"
    );
    if let Some(frame) = &frames[1] {
        let decoded = Private::decode(frame.as_ref()).expect("a private frame");
        assert!(decoded.game.is_empty(), "{name}");
        assert_eq!(
            decoded.encode_to_vec(),
            frame.to_vec(),
            "{name}: no field 4"
        );
    }
}

/// A game that keeps the default hook: frames exactly as before.
#[test]
fn the_default_hook_changes_no_frame() {
    let f = Fixture::default;
    check_default("open", OpenRoom::with_game(f()), false);
    check_default("aoi", AoiRoom::with_game(f(), Grid2::new(20.0)), true);
    check_default(
        "team",
        TeamRoom::with_game(f(), VisionGrid2::<Position>::new(10.0)),
        false,
    );
    check_default(
        "team (delta)",
        TeamRoom::with_game(f(), VisionGrid2::<Position>::new(10.0)).with_delta(),
        true,
    );
    check_default("pvs", SectorRoom::with_game(f(), fixture_map()), false);
    check_default("sharded", sharded(f()), false);
    check_default(
        "sharded × spatial",
        ShardedSpatialRoom::with_shard(sharded(f()), Grid2::new(20.0)),
        true,
    );
}
