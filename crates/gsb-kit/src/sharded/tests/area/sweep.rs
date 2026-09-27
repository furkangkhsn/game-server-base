//! The actor tests' game: the fixture game whose players spawn where
//! their identity says (`x:y`) and move by one input (a teleport), and
//! whose systems ask the seam for the disc of [`RADIUS`] around the
//! map's centre every tick, reporting each answer on a feed.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::channel::Mailbox;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};

use super::Spot;
use crate::game::{Game, InputSeq, ShardGame};
use crate::sharded::{Found, Holder, Seam};
use crate::testing::{DEFAULT_SPEED, FixCodec, FixMig, Fixture, Position, Speed, WirePos};

/// Every shard's probe: the disc of this radius around the centre.
pub(super) const RADIUS: f32 = 20.0;
/// The teleport input: `x, y` as two little-endian `f32`s.
pub(super) const MOVE: u16 = 1960;

/// What one shard's probe found in one tick: `(wire, holder, spot)`,
/// in wire order.
#[derive(Debug, Clone)]
pub(super) struct Sample {
    pub(super) shard: usize,
    pub(super) tick: u64,
    pub(super) found: Vec<(u64, Holder, [f32; 2])>,
}

/// The fixture game with identity spawns (`x:y`), the teleport input
/// and the probe.
pub(super) struct Sweep {
    game: Fixture,
    index: usize,
    feed: Mailbox<Sample>,
    buf: Vec<Found<Spot>>,
}

impl Sweep {
    /// Shard `index`'s game, reporting on `feed`.
    pub(super) fn new(index: usize, feed: Mailbox<Sample>) -> Self {
        Self {
            game: Fixture::default(),
            index,
            feed,
            buf: Vec::new(),
        }
    }
}

/// A login's spot: its identity `x:y`.
pub(super) fn parse(identity: &str) -> Position {
    let (x, y) = identity.split_once(':').expect("x:y");
    Position {
        x: x.parse().expect("x"),
        y: y.parse().expect("y"),
    }
}

impl Game for Sweep {
    type Codec = FixCodec;
    const SNAPSHOT_OP: u16 = Fixture::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = Fixture::PRIVATE_OP;
    fn codec(&self) -> &FixCodec {
        self.game.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.game.spawn_player(world, conn)
    }
    fn spawn_player_as(&mut self, world: &mut World, _: ConnectionId, identity: &str) -> Entity {
        world.spawn((parse(identity), Speed(DEFAULT_SPEED))).id()
    }
    fn ingest(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        _seq: &mut InputSeq,
    ) {
        for a in actions.drain(..) {
            let (Some(&e), true) = (players.get(&a.player), a.op == MOVE) else {
                continue;
            };
            let x = f32::from_le_bytes(a.payload[..4].try_into().expect("4 bytes"));
            let y = f32::from_le_bytes(a.payload[4..8].try_into().expect("4 bytes"));
            world.entity_mut(e).insert(Position { x, y });
        }
    }
    fn systems(&mut self, _world: &mut World, _ctx: &TickCtx) {}
}

impl ShardGame for Sweep {
    type Mig = FixMig;
    fn capture(&self, world: &World, entity: Entity) -> FixMig {
        self.game.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: FixMig) -> Entity {
        self.game.restore(world, mig)
    }
    fn systems_seam(&mut self, world: &mut World, ctx: &TickCtx, seam: &mut Seam<'_, '_, WirePos>) {
        seam.within::<Spot>(world, [0.0, 0.0], RADIUS, &mut self.buf);
        let found = self.buf.iter().map(|f| (f.wire, f.holder, f.view.0));
        let sample = Sample {
            shard: self.index,
            tick: ctx.tick,
            found: found.collect(),
        };
        self.feed.try_send(sample).expect("the feed has room");
    }
}
