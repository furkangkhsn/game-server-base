//! The kit's fixture game: the smallest complete [`Game`] the kit's
//! in-module tests drive the generic rooms with (the kit never sees a
//! real game — KIT-ARCHITECTURE §3).
//!
//! One entity per player on a 2D plane: a `Position` (the broadcast
//! marker), a `Speed`, an optional `MoveTarget`; nothing moves unless a
//! test writes a position (no input decoding, no systems — the tests
//! place entities by hand). The record is the TRUNCATED position under
//! the entity's wire id (`{ uint64 entity = 1; sint32 x = 2; sint32 y =
//! 3; }`), decoded by the typed mirrors below; teams are conn parity;
//! what migrates is position, speed and target; the PVS map is four
//! convex sectors.
//!
//! The tests that pin a real game's record VALUES (decoded coordinates,
//! quantization) are not here: they run on the demo game in `gsb-demo`
//! against its own codec.

use std::collections::HashMap;

use bevy_ecs::prelude::{Changed, Component, Entity, World};
use bytes::BytesMut;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use prost::Message;

use crate::codec::RecordCodec;
use crate::common::InputSeq;
use crate::game::{Game, ShardGame, TeamGame};
use crate::space::Planar;
use crate::team::Team;

mod mirror;
mod rooms;

pub(crate) use mirror::*;
pub(crate) use rooms::*;

/// Units per second of a player's `Speed`.
pub(crate) const DEFAULT_SPEED: f32 = 10.0;

/// Position on the plane (the broadcast marker and every simulation
/// preset's input).
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
pub(crate) struct Position {
    pub x: f32,
    pub y: f32,
}

impl Planar for Position {
    type Coord = f32;
    fn planar(&self) -> [f32; 2] {
        [self.x, self.y]
    }
}

/// A pending move target (carried across a shard border; never acted
/// on — the fixture has no movement system).
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
pub(crate) struct MoveTarget {
    pub x: f32,
    pub y: f32,
}

/// A player's speed (players have one, NPCs the tests spawn may not).
#[derive(Debug, Clone, Copy, PartialEq, Component)]
pub(crate) struct Speed(pub f32);

/// The fixture's wire value: the truncated position (also the sharded
/// rooms' border-strip payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WirePos {
    pub x: i32,
    pub y: i32,
}

impl Planar for WirePos {
    type Coord = i32;
    fn planar(&self) -> [i32; 2] {
        [self.x, self.y]
    }
}

/// The fixture's record codec.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FixCodec;

impl RecordCodec for FixCodec {
    type Marker = Position;
    type Query = &'static Position;
    type Dirty = Changed<Position>;
    type Wire = WirePos;

    fn wire(&self, pos: &Position) -> WirePos {
        WirePos {
            x: pos.x as i32,
            y: pos.y as i32,
        }
    }

    fn encode(&self, id: u64, &WirePos { x, y }: &WirePos, out: &mut BytesMut) {
        Record { entity: id, x, y }
            .encode(out)
            .expect("protobuf encode into an in-memory buffer failed");
    }
}

// ── The game ──────────────────────────────────────────────────────────

/// What a fixture entity carries across a shard border.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FixMig {
    pub pos: Position,
    pub speed: Option<f32>,
    pub target: Option<MoveTarget>,
}

/// The fixture game (module docs).
#[derive(Debug, Default)]
pub(crate) struct Fixture {
    codec: FixCodec,
}

impl Game for Fixture {
    type Codec = FixCodec;

    fn codec(&self) -> &FixCodec {
        &self.codec
    }

    /// Every player spawns at the origin; the tests place it.
    fn spawn_player(&mut self, world: &mut World, _conn: ConnectionId) -> Entity {
        world
            .spawn((Position::default(), Speed(DEFAULT_SPEED)))
            .id()
    }

    /// The fixture decodes no input.
    fn ingest(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        actions: &mut Vec<Action>,
        _players: &HashMap<PlayerId, Entity>,
        _seq: &mut InputSeq,
    ) {
        actions.clear();
    }

    /// The fixture has no systems.
    fn systems(&mut self, _world: &mut World, _ctx: &TickCtx) {}
}

/// Conn parity: team 0, 1, 0, 1, …
impl TeamGame for Fixture {
    fn team_of(&mut self, _world: &World, conn: ConnectionId, _entity: Entity) -> Team {
        Team((conn.0 % 2) as u8)
    }
}

impl ShardGame for Fixture {
    type Mig = FixMig;

    fn capture(&self, world: &World, entity: Entity) -> FixMig {
        let e = world.entity(entity);
        FixMig {
            pos: e.get::<Position>().copied().unwrap_or_default(),
            speed: e.get::<Speed>().map(|s| s.0),
            target: e.get::<MoveTarget>().copied(),
        }
    }

    fn restore(&mut self, world: &mut World, mig: FixMig) -> Entity {
        let entity = match mig.speed {
            Some(speed) => world.spawn((mig.pos, Speed(speed))).id(),
            None => world.spawn(mig.pos).id(),
        };
        if let Some(target) = mig.target {
            world.entity_mut(entity).insert(target);
        }
        entity
    }
}
