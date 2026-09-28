//! The kit's 3D fixture game (BACKLOG A5): the fixture game's shape in
//! space, for the volumetric presets' room tests — one entity per player
//! with a [`Position3`] (the broadcast marker; the third axis is
//! height), the truncated position as the record (`{ uint64 entity = 1;
//! sint32 x = 2; sint32 y = 3; sint32 z = 4; }`), the position as what
//! migrates, and nothing that moves unless a test writes a position.

use std::collections::HashMap;

use bevy_ecs::prelude::{Changed, Component, Entity, World};
use bytes::BytesMut;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use prost::Message;

use crate::codec::RecordCodec;
use crate::common::InputSeq;
use crate::game::{Game, ShardGame};
use crate::space::Spatial;

/// Position in space.
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
pub(crate) struct Position3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Spatial for Position3 {
    type Coord = f32;
    fn spatial(&self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }
}

/// The wire value: the truncated position (also the strip payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WirePos3 {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl Spatial for WirePos3 {
    type Coord = i32;
    fn spatial(&self) -> [i32; 3] {
        [self.x, self.y, self.z]
    }
}

/// The record codec.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Codec3;

impl RecordCodec for Codec3 {
    type Marker = Position3;
    type Query = &'static Position3;
    type Dirty = Changed<Position3>;
    type Wire = WirePos3;

    fn wire(&self, p: &Position3) -> WirePos3 {
        WirePos3 {
            x: p.x as i32,
            y: p.y as i32,
            z: p.z as i32,
        }
    }

    fn encode(&self, id: u64, &WirePos3 { x, y, z }: &WirePos3, out: &mut BytesMut) {
        Record3 {
            entity: id,
            x,
            y,
            z,
        }
        .encode(out)
        .expect("protobuf encode into an in-memory buffer failed");
    }
}

/// The 3D fixture game (module docs).
#[derive(Debug, Default)]
pub(crate) struct Fixture3 {
    codec: Codec3,
}

impl Game for Fixture3 {
    type Codec = Codec3;

    const SNAPSHOT_OP: u16 = 1903;
    const PRIVATE_OP: u16 = 1904;

    fn codec(&self) -> &Codec3 {
        &self.codec
    }

    /// Every player spawns at the origin; the tests place it.
    fn spawn_player(&mut self, world: &mut World, _conn: ConnectionId) -> Entity {
        world.spawn(Position3::default()).id()
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

impl ShardGame for Fixture3 {
    type Mig = Position3;

    fn capture(&self, world: &World, entity: Entity) -> Position3 {
        world
            .entity(entity)
            .get::<Position3>()
            .copied()
            .unwrap_or_default()
    }

    fn restore(&mut self, world: &mut World, mig: Position3) -> Entity {
        world.spawn(mig).id()
    }
}

// ── The typed mirrors ─────────────────────────────────────────────────

/// One entity record (the codec's body).
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Record3 {
    #[prost(uint64, tag = "1")]
    pub entity: u64,
    #[prost(sint32, tag = "2")]
    pub x: i32,
    #[prost(sint32, tag = "3")]
    pub y: i32,
    #[prost(sint32, tag = "4")]
    pub z: i32,
}

/// One cell exit (the `Grid3` preset's body).
#[derive(Clone, PartialEq, Message)]
pub(crate) struct CellExit3 {
    #[prost(sint32, tag = "1")]
    pub x: i32,
    #[prost(sint32, tag = "2")]
    pub y: i32,
    #[prost(sint32, tag = "3")]
    pub z: i32,
}

/// The kit's `WorldSnapshot` over the 3D record and cell exit.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct WorldSnapshot3 {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
    #[prost(message, repeated, tag = "2")]
    pub entities: Vec<Record3>,
    #[prost(uint64, repeated, tag = "3")]
    pub removed: Vec<u64>,
    #[prost(message, repeated, tag = "4")]
    pub cell_exits: Vec<CellExit3>,
    #[prost(bool, tag = "5")]
    pub delta: bool,
}
