//! The fixture games with a CENTIMETRE wire (BACKLOG A7): the 2D and 3D
//! fixture games whose record is the metre position ×100, rounded — a
//! wire finer than the position, projected in its own unit (the
//! fixtures' `WirePos` / `WirePos3` report their coordinates as they
//! are).

use std::collections::HashMap;

use bevy_ecs::prelude::{Changed, Entity, World};
use bytes::BytesMut;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};

use crate::codec::RecordCodec;
use crate::game::{Game, InputSeq, ShardGame};
use crate::testing::{Codec3, FixCodec, Position, Position3, WirePos, WirePos3};

/// Metres → centimetres, rounded.
pub(super) fn cm(v: f32) -> i32 {
    (v * 100.0).round() as i32
}

/// The 2D fixture record in centimetres (the fixture's bytes).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CmCodec;

impl RecordCodec for CmCodec {
    type Marker = Position;
    type Query = &'static Position;
    type Dirty = Changed<Position>;
    type Wire = WirePos;

    fn wire(&self, p: &Position) -> WirePos {
        WirePos {
            x: cm(p.x),
            y: cm(p.y),
        }
    }

    fn encode(&self, id: u64, wire: &WirePos, out: &mut BytesMut) {
        FixCodec.encode(id, wire, out);
    }
}

/// The 3D fixture record in centimetres (the 3D fixture's bytes).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CmCodec3;

impl RecordCodec for CmCodec3 {
    type Marker = Position3;
    type Query = &'static Position3;
    type Dirty = Changed<Position3>;
    type Wire = WirePos3;

    fn wire(&self, p: &Position3) -> WirePos3 {
        WirePos3 {
            x: cm(p.x),
            y: cm(p.y),
            z: cm(p.z),
        }
    }

    fn encode(&self, id: u64, wire: &WirePos3, out: &mut BytesMut) {
        Codec3.encode(id, wire, out);
    }
}

/// Fixture game `G` with record codec `C`: players spawn as `G`'s, the
/// tests place them, and what migrates is `G`'s.
#[derive(Debug, Default)]
pub(super) struct Centi<G, C> {
    game: G,
    codec: C,
}

impl<G: Game, C: RecordCodec + Send + 'static> Game for Centi<G, C> {
    type Codec = C;
    const SNAPSHOT_OP: u16 = G::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = G::PRIVATE_OP;

    fn codec(&self) -> &C {
        &self.codec
    }

    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.game.spawn_player(world, conn)
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

    fn systems(&mut self, _world: &mut World, _ctx: &TickCtx) {}
}

impl<G: ShardGame, C: RecordCodec + Send + 'static> ShardGame for Centi<G, C> {
    type Mig = G::Mig;

    fn capture(&self, world: &World, entity: Entity) -> G::Mig {
        self.game.capture(world, entity)
    }

    fn restore(&mut self, world: &mut World, mig: G::Mig) -> Entity {
        self.game.restore(world, mig)
    }
}
