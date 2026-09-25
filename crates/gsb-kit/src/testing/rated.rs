//! The fixture's record with a SEND RATE (`RecordCodec::send_every`,
//! A10): the same wire value and bytes as the wrapped codec (either
//! framing), plus a class the record takes from its own value — the x
//! coordinate's band of 16 units, cycling every step, 2nd, 4th, 8th —
//! so a record walking across the map changes class now and then.

use std::collections::HashMap;

use bevy_ecs::prelude::{Changed, Entity, World};
use bytes::BytesMut;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};

use crate::codec::{RecordCodec, SendEvery};
use crate::common::InputSeq;
use crate::game::Game;

use super::{FixCodec, Fixture, Position, WirePos};

/// `C` (the fixture's `FixCodec` or `PackedCodec`) with [`band_class`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Rated<C>(pub C);

/// The class of a record at `wire`: by the x band, `[0, 16)` every step,
/// `[16, 32)` every 2nd, `[32, 48)` every 4th, `[48, 64)` every 8th,
/// then again (negative x: `[-16, 0)` every 8th, …).
pub(crate) fn band_class(wire: &WirePos) -> SendEvery {
    match wire.x.div_euclid(16).rem_euclid(4) {
        0 => SendEvery::Tick,
        1 => SendEvery::Ticks2,
        2 => SendEvery::Ticks4,
        _ => SendEvery::Ticks8,
    }
}

/// The longest class [`band_class`] gives (its staleness bound is one
/// step less).
pub(crate) const BAND_MAX: SendEvery = SendEvery::Ticks8;

impl<C> RecordCodec for Rated<C>
where
    C: RecordCodec<
            Marker = Position,
            Query = &'static Position,
            Dirty = Changed<Position>,
            Wire = WirePos,
        >,
{
    type Marker = Position;
    type Query = &'static Position;
    type Dirty = Changed<Position>;
    type Wire = WirePos;

    const RUN: bool = C::RUN;

    fn wire(&self, pos: &Position) -> WirePos {
        self.0.wire(pos)
    }

    fn encode(&self, id: u64, wire: &WirePos, out: &mut BytesMut) {
        self.0.encode(id, wire, out);
    }

    fn send_every(&self, wire: &WirePos) -> SendEvery {
        band_class(wire)
    }
}

/// The fixture game over `Rated<FixCodec>`: players spawn at the origin
/// and the tests place them; nothing else moves.
#[derive(Debug, Default)]
pub(crate) struct RatedFixture {
    fixture: Fixture,
    codec: Rated<FixCodec>,
}

impl Game for RatedFixture {
    type Codec = Rated<FixCodec>;

    const SNAPSHOT_OP: u16 = <Fixture as Game>::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = <Fixture as Game>::PRIVATE_OP;

    fn codec(&self) -> &Rated<FixCodec> {
        &self.codec
    }

    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.fixture.spawn_player(world, conn)
    }

    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        self.fixture.ingest(world, ctx, actions, players, seq);
    }

    fn systems(&mut self, _world: &mut World, _ctx: &TickCtx) {}
}
