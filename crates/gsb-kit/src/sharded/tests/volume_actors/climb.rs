//! The actor tests' game: the 3D fixture game whose players spawn where
//! their identity says (`x:y:z`) and move by one input (a teleport), and
//! whose systems report every tick every entity the shard's seam holds.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::channel::Mailbox;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};

use super::Spot3;
use crate::game::{Game, InputSeq, ShardGame};
use crate::sharded::{Found, Holder, Seam};
use crate::testing::{Codec3, Fixture3, Position3, WirePos3};

/// The teleport input: `x, y, z` as three little-endian `f32`s.
pub(super) const MOVE: u16 = 1961;

/// What one shard's seam held in one tick: `(wire, holder, spot)`, in
/// wire order.
#[derive(Debug, Clone)]
pub(super) struct Sample {
    pub(super) shard: usize,
    pub(super) tick: u64,
    pub(super) found: Vec<(u64, Holder, [f32; 3])>,
}

/// The 3D fixture game with identity spawns, the teleport and the
/// report.
pub(super) struct Climb {
    game: Fixture3,
    index: usize,
    feed: Mailbox<Sample>,
    buf: Vec<Found<Spot3>>,
}

impl Climb {
    /// Shard `index`'s game, reporting on `feed`.
    pub(super) fn new(index: usize, feed: Mailbox<Sample>) -> Self {
        Self {
            game: Fixture3::default(),
            index,
            feed,
            buf: Vec::new(),
        }
    }
}

/// A login's spot: its identity `x:y:z`.
pub(super) fn parse(identity: &str) -> Position3 {
    let mut axes = identity.split(':').map(|v| v.parse().expect("a number"));
    let mut next = || axes.next().expect("x:y:z");
    Position3 {
        x: next(),
        y: next(),
        z: next(),
    }
}

impl Game for Climb {
    type Codec = Codec3;
    const SNAPSHOT_OP: u16 = Fixture3::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = Fixture3::PRIVATE_OP;
    fn codec(&self) -> &Codec3 {
        self.game.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.game.spawn_player(world, conn)
    }
    fn spawn_player_as(&mut self, world: &mut World, _: ConnectionId, identity: &str) -> Entity {
        world.spawn(parse(identity)).id()
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
            let axis = |i: usize| {
                let bytes = a.payload[4 * i..4 * i + 4].try_into().expect("4 bytes");
                f32::from_le_bytes(bytes)
            };
            let (x, y, z) = (axis(0), axis(1), axis(2));
            world.entity_mut(e).insert(Position3 { x, y, z });
        }
    }
    fn systems(&mut self, _world: &mut World, _ctx: &TickCtx) {}
}

impl ShardGame for Climb {
    type Mig = Position3;
    fn capture(&self, world: &World, entity: Entity) -> Position3 {
        self.game.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: Position3) -> Entity {
        self.game.restore(world, mig)
    }
    fn systems_seam(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        seam: &mut Seam<'_, '_, WirePos3>,
    ) {
        seam.area::<Spot3>(world, |_| true, &mut self.buf);
        let found = self.buf.iter().map(|f| (f.wire, f.holder, f.view.0));
        let sample = Sample {
            shard: self.index,
            tick: ctx.tick,
            found: found.collect(),
        };
        self.feed.try_send(sample).expect("the feed has room");
    }
}
