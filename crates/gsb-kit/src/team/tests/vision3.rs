//! Team fog in 3D (KIT-ARCHITECTURE §12: the arena demo's visibility
//! model): the team room over the kit's `VisionGrid3`, with more than
//! two teams, where HEIGHT decides what a team sees — a unit straight
//! above an enemy, beyond the radius, is not in its package although
//! the two share a ground-plane spot.

use std::collections::HashMap;

use bevy_ecs::prelude::{Changed, Component, Entity};
use bytes::BytesMut;
use gsb_core::room::Action;

use super::*;
use crate::codec::RecordCodec;
use crate::game::{Game, InputSeq, TeamGame};
use crate::space::{Spatial, VisionGrid3};

/// A 3D position (y is height).
#[derive(Debug, Clone, Copy, PartialEq, Default, Component)]
struct P3 {
    x: f32,
    y: f32,
    z: f32,
}

impl Spatial for P3 {
    type Coord = f32;
    fn spatial(&self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }
}

/// The record: the id and the three truncated coordinates.
struct Codec3;

impl RecordCodec for Codec3 {
    type Marker = P3;
    type Query = &'static P3;
    type Dirty = Changed<P3>;
    type Wire = [i32; 3];

    fn wire(&self, p: &P3) -> [i32; 3] {
        [p.x as i32, p.y as i32, p.z as i32]
    }

    fn encode(&self, id: u64, w: &[i32; 3], out: &mut BytesMut) {
        prost::encoding::uint64::encode(1, &id, out);
        for (tag, v) in (2..).zip(w) {
            prost::encoding::sint32::encode(tag, v, out);
        }
    }
}

/// A three-team 3D game: players spawn at the origin (the test places
/// them), teams are conn mod 3.
struct Arena3(Codec3);

impl Game for Arena3 {
    type Codec = Codec3;

    const SNAPSHOT_OP: u16 = 1901;
    const PRIVATE_OP: u16 = 1902;

    fn codec(&self) -> &Codec3 {
        &self.0
    }
    fn spawn_player(&mut self, world: &mut World, _conn: ConnectionId) -> Entity {
        world.spawn(P3::default()).id()
    }
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
    fn systems(&mut self, _world: &mut World, _ctx: &TickCtx) {}
}

impl TeamGame for Arena3 {
    fn team_of(&mut self, _world: &World, conn: ConnectionId, _entity: Entity) -> Team {
        Team((conn.0 % 3) as u8)
    }
}

/// Three teams under a 25-unit 3D vision radius. A (team 0) at the
/// origin; B (team 1) 30 straight above it; C (team 2) at (15, 0, 15),
/// ≈ 21.2 from A; D (team 1) at (0, 0, 24). Team 0 sees C and D but not
/// B (the planar distance A–B is 0 — only height separates them); team
/// 1 sees everyone through D; team 2 sees A and D, not B (≈ 36.7 away).
#[test]
fn team_fog_in_3d_separates_by_height_across_three_teams() {
    let mut world = World::new();
    let mut room = super::super::TeamRoom::with_game(Arena3(Codec3), VisionGrid3::<P3>::new(25.0));
    let mut place = |world: &mut World, conn: u64, x: f32, y: f32, z: f32| {
        let admission = room.on_join(world, ConnectionId(conn));
        let entity = room.player_entity[&admission.player];
        world.entity_mut(entity).insert(P3 { x, y, z });
        admission.entity
    };
    let a = place(&mut world, 3, 0.0, 0.0, 0.0); // team 0
    let b = place(&mut world, 1, 0.0, 30.0, 0.0); // team 1
    let c = place(&mut world, 2, 15.0, 0.0, 15.0); // team 2
    let d = place(&mut world, 4, 0.0, 0.0, 24.0); // team 1
    room.update(&mut world, &ctx(1));

    let mut package = |team: u8| {
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Team(team), &[], &mut out));
        // The record bodies are this test's own codec: read the ids
        // through the kit's opaque envelope.
        crate::proto::WorldSnapshot::decode(out.as_ref())
            .expect("snapshot")
            .entities
            .iter()
            .map(|body| {
                prost::encoding::decode_varint(&mut &body[1..]).expect("the id field (tag 1)")
            })
            .collect::<BTreeSet<u64>>()
    };
    assert_eq!(
        package(0),
        BTreeSet::from([a, c, d]),
        "team 0: not B above A"
    );
    assert_eq!(package(1), BTreeSet::from([a, b, c, d]), "team 1");
    assert_eq!(package(2), BTreeSet::from([a, c, d]), "team 2");
}
