//! The fixture with a light (the lit AOI room's tests, A9): a player
//! carrying [`Facing`] sees, of its neighbourhood, itself and the records
//! AHEAD of it along x — a light cone reduced to a half-plane — except a
//! [`Cloaked`] one; a player without `Facing` has no light.

use std::collections::HashMap;

use bevy_ecs::prelude::{Component, Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};

use crate::common::InputSeq;
use crate::game::{Game, LitGame};
use crate::testing::{Fixture, Position, WirePos};

/// The direction a viewer faces along x (`1`: east, `-1`: west).
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub(crate) struct Facing(pub i32);

/// A record no light shows (a stealth rule: decided from the record's
/// ENTITY, not from its wire value).
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub(crate) struct Cloaked;

/// The fixture game with the half-plane light (module docs).
#[derive(Debug, Default)]
pub(crate) struct Lamp(pub Fixture);

impl Game for Lamp {
    type Codec = <Fixture as Game>::Codec;

    const SNAPSHOT_OP: u16 = Fixture::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = Fixture::PRIVATE_OP;

    fn codec(&self) -> &Self::Codec {
        self.0.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.0.spawn_player(world, conn)
    }
    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        self.0.ingest(world, ctx, actions, players, seq);
    }
    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.0.systems(world, ctx);
    }
}

/// The viewer's light: its entity, its wire x and its facing.
pub(crate) struct Beam {
    me: Entity,
    x: i32,
    dir: i32,
}

impl LitGame for Lamp {
    type Light = Beam;

    fn light(&self, world: &World, viewer: Entity) -> Option<Beam> {
        let dir = world.get::<Facing>(viewer)?.0;
        let x = world.get::<Position>(viewer)?.x as i32;
        Some(Beam { me: viewer, x, dir })
    }

    fn lit(&self, beam: &Beam, world: &World, record: Entity, wire: &WirePos) -> bool {
        record == beam.me
            || ((wire.x - beam.x) * beam.dir > 0 && !world.entity(record).contains::<Cloaked>())
    }
}
