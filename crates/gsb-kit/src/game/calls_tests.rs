//! The call-table hooks reach the game through the kit rooms (BACKLOG
//! B151): `boot_calls`, `update_with_calls` (→ `Game::systems_with_calls`),
//! `call_owner` (with the player's entity) and `owner_alive`, forwarded
//! by the open, AOI and sharded rooms the same way — and the event-table
//! hooks (round e5): `boot_events`, `update_with_events`
//! (→ `Game::systems_with_events`).

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::calls::{Ask, BootError, Call, CallSpec, Calls, Mail, Owner, Request, Route};
use gsb_core::channel::Inbox;
use gsb_core::events::{Event, Events};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::room::{Action, GameLogic, TickCtx};

use crate::aoi::AoiRoom;
use crate::common::InputSeq;
use crate::game::{Game, ShardGame};
use crate::room::OpenRoom;
use crate::sharded::ShardedRoom;
use crate::space::{Grid2, GridPartition2};
use crate::testing::{FixCodec, FixMig, Fixture, Position};

/// A client request the game takes itself.
#[derive(Clone, PartialEq, prost::Message)]
struct Ping {
    #[prost(uint32, tag = "1")]
    n: u32,
}
impl Call for Ping {
    type Response = Ping;
    type Error = Ping;
}
impl Request for Ping {
    const OP: u16 = 4242;
}

/// An event the game takes.
#[derive(Clone, PartialEq, prost::Message)]
struct Gift {
    #[prost(uint32, tag = "1")]
    n: u32,
}
impl gsb_core::events::Event for Gift {
    const OP: u16 = 4243;
    const TO: gsb_core::events::Dest = gsb_core::events::Dest::World;
}
impl gsb_core::events::WorldEvent for Gift {}

/// A service call the game issues from its systems.
#[derive(Clone, Debug)]
struct Note;
impl Call for Note {
    type Response = ();
    type Error = ();
}

/// The fixture game plus the call hooks.
struct Calling {
    inner: Fixture,
    note: Route<Ask<Note>>,
    systems: u32,
    with_calls: u32,
    with_events: u32,
}

impl Calling {
    fn new() -> (Self, Inbox<Mail<Ask<Note>>>) {
        let (note, inbox) = Route::new("note", 8);
        (
            Self {
                inner: Fixture::default(),
                note,
                systems: 0,
                with_calls: 0,
                with_events: 0,
            },
            inbox,
        )
    }
}

impl Game for Calling {
    type Codec = FixCodec;
    const SNAPSHOT_OP: u16 = 1901;
    const PRIVATE_OP: u16 = 1902;

    fn codec(&self) -> &FixCodec {
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
    fn systems(&mut self, _world: &mut World, _ctx: &TickCtx) {
        self.systems += 1;
    }
    fn boot_calls(&mut self, calls: &mut Calls) -> Result<(), BootError> {
        calls.accept::<Ping>()
    }
    fn systems_with_calls(&mut self, _world: &mut World, _ctx: &TickCtx, calls: &mut Calls) {
        self.with_calls += 1;
        calls.call(CallSpec::new(&self.note, Note, |a| a));
    }
    fn boot_events(&mut self, events: &mut Events) -> Result<(), BootError> {
        events.accept::<Gift>()
    }
    fn systems_with_events(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        _calls: &mut Calls,
        events: &mut Events,
    ) {
        self.with_events += 1;
        assert!(events.handles(Gift::OP), "the room's own table");
    }
    fn call_owner(
        &self,
        _world: &World,
        player: PlayerId,
        entity: Option<Entity>,
    ) -> Option<Owner> {
        match entity {
            Some(e) => Some(Owner(e.to_bits())),
            None => Some(player.into()),
        }
    }
    fn owner_alive(&self, _world: &World, owner: Owner) -> Option<bool> {
        Some(owner != Owner(999))
    }
}

impl ShardGame for Calling {
    type Mig = FixMig;
    fn capture(&self, world: &World, entity: Entity) -> FixMig {
        self.inner.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: FixMig) -> Entity {
        self.inner.restore(world, mig)
    }
}

fn ctx() -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick: 1,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: Default::default(),
        paths: Default::default(),
        time: Duration::ZERO,
        rng: Default::default(),
    }
}

/// Every hook, through one room; `game` reads the game's counters back.
fn drive<R: GameLogic<World>>(mut room: R, game: impl Fn(&R) -> (u32, u32, u32)) {
    let mut world = World::new();
    let mut calls = Calls::default();
    room.boot_calls(&mut calls).expect("boot");
    assert!(calls.handles(Ping::OP), "boot_calls reached the game");
    let mut events = Events::default();
    room.boot_events(&mut events).expect("boot");
    assert!(events.handles(Gift::OP), "boot_events reached the game");

    let admission = room.on_join(&mut world, ConnectionId(1));
    let owner = room.call_owner(&world, admission.player).expect("an owner");
    assert_ne!(
        owner,
        Owner::from(admission.player),
        "the entity's, not the player's"
    );
    assert_eq!(
        room.call_owner(&world, PlayerId(77)),
        Some(Owner::from(PlayerId(77)))
    );
    assert_eq!(room.owner_alive(&world, Owner(999)), Some(false));
    assert_eq!(room.owner_alive(&world, owner), Some(true));

    room.update_with_calls(&mut world, &ctx(), &mut calls);
    assert_eq!(
        game(&room),
        (0, 1, 0),
        "systems_with_calls ran, not systems"
    );
    assert_eq!(calls.pending_len(), 1, "the game's call is in the table");
    room.update(&mut world, &ctx());
    assert_eq!(game(&room), (1, 1, 0), "a plain update runs systems");
    room.update_with_events(&mut world, &ctx(), &mut calls, &mut events);
    assert_eq!(game(&room), (1, 1, 1), "systems_with_events ran, alone");
}

#[test]
fn the_open_room_forwards_the_call_hooks() {
    let (game, _inbox) = Calling::new();
    drive(OpenRoom::with_game(game), |r| {
        (r.game().systems, r.game().with_calls, r.game().with_events)
    });
}

#[test]
fn the_aoi_room_forwards_the_call_hooks() {
    let (game, _inbox) = Calling::new();
    drive(AoiRoom::with_game(game, Grid2::new(20.0)), |r| {
        (r.game().systems, r.game().with_calls, r.game().with_events)
    });
}

#[test]
fn the_sharded_room_forwards_the_call_hooks() {
    let (game, _inbox) = Calling::new();
    let room: ShardedRoom<Calling, GridPartition2<Position>> =
        ShardedRoom::with_game(game, GridPartition2::new(1, 50.0), 0);
    drive(room, |r| {
        (r.game().systems, r.game().with_calls, r.game().with_events)
    });
}
