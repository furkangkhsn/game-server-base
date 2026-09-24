//! Test-only games for the kit's in-module tests: wrappers around a
//! game that add one behaviour a test needs (compiled for tests only).

use std::collections::HashMap;

use bevy_ecs::prelude::{Component, Entity, With, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};

use crate::kit::common::InputSeq;
use crate::kit::game::{Game, ShardGame};

/// Marks an entity the [`Culling`] game despawns in its next systems
/// run.
#[derive(Debug, Clone, Copy, Component)]
pub(crate) struct Doomed;

/// A game whose systems despawn every [`Doomed`] entity before running
/// the wrapped game's systems — GAME code despawning an entity (an NPC
/// dying), with no leave and no migration involved.
pub(crate) struct Culling<G>(pub G);

impl<G: Game> Game for Culling<G> {
    type Codec = G::Codec;

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
        let doomed: Vec<Entity> = world
            .query_filtered::<Entity, With<Doomed>>()
            .iter(world)
            .collect();
        for entity in doomed {
            world.despawn(entity);
        }
        self.0.systems(world, ctx);
    }
}

impl<G: ShardGame> ShardGame for Culling<G> {
    type Mig = G::Mig;

    fn capture(&self, world: &World, entity: Entity) -> G::Mig {
        self.0.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: G::Mig) -> Entity {
        self.0.restore(world, mig)
    }
}

mod requests;
