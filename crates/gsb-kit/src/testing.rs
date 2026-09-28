//! Test-only games for the kit's in-module tests (compiled for tests
//! only): the fixture game (`fixture`), its record in the record run
//! (`packed`), its record with a send rate (`rated`), its 3D sibling
//! (`volume`) and wrappers around a game that add one behaviour a test
//! needs.

use std::collections::HashMap;

use bevy_ecs::prelude::{Component, Entity, With, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};

use crate::common::InputSeq;
use crate::game::{Game, ShardGame, TeamGame};
use crate::team::Team;

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

    const SNAPSHOT_OP: u16 = G::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = G::PRIVATE_OP;

    fn codec(&self) -> &Self::Codec {
        self.0.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.0.spawn_player(world, conn)
    }
    fn spawn_player_as(&mut self, world: &mut World, conn: ConnectionId, identity: &str) -> Entity {
        self.0.spawn_player_as(world, conn, identity)
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

/// Marks an entity the [`Vetoing`] game refuses to release from a
/// disconnect hold (it is "in combat").
#[derive(Debug, Clone, Copy, Component)]
pub(crate) struct InCombat;

/// A game whose [`Game::may_release`] vetoes ending a hold while the
/// parked entity carries [`InCombat`] — the combat veto of RECONNECT
/// §14.4/§17.
pub(crate) struct Vetoing<G>(pub G);

impl<G: Game> Game for Vetoing<G> {
    type Codec = G::Codec;

    const SNAPSHOT_OP: u16 = G::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = G::PRIVATE_OP;

    fn codec(&self) -> &Self::Codec {
        self.0.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.0.spawn_player(world, conn)
    }
    fn spawn_player_as(&mut self, world: &mut World, conn: ConnectionId, identity: &str) -> Entity {
        self.0.spawn_player_as(world, conn, identity)
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
    fn may_release(&mut self, world: &mut World, entity: Entity) -> bool {
        !world.entity(entity).contains::<InCombat>()
    }
}

impl<G: TeamGame> TeamGame for Vetoing<G> {
    fn team_of(&mut self, world: &World, conn: ConnectionId, entity: Entity) -> Team {
        self.0.team_of(world, conn, entity)
    }
}

impl<G: ShardGame> ShardGame for Vetoing<G> {
    type Mig = G::Mig;

    fn capture(&self, world: &World, entity: Entity) -> G::Mig {
        self.0.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: G::Mig) -> Entity {
        self.0.restore(world, mig)
    }
}

/// Marks an entity the [`Kicker`] game kicks in its next systems run,
/// with the reason it carries.
#[derive(Debug, Clone, Component)]
pub(crate) struct KickMe(pub &'static str);

/// A game whose systems kick every [`KickMe`] entity through the kit's
/// verb ([`crate::game::kick`], E8) — the marker is removed — before
/// running the wrapped game's systems.
pub(crate) struct Kicker<G>(pub G);

impl<G: Game> Game for Kicker<G> {
    type Codec = G::Codec;

    const SNAPSHOT_OP: u16 = G::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = G::PRIVATE_OP;

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
        let marked: Vec<(Entity, &'static str)> = world
            .query::<(Entity, &KickMe)>()
            .iter(world)
            .map(|(e, k)| (e, k.0))
            .collect();
        for (entity, reason) in marked {
            world.entity_mut(entity).remove::<KickMe>();
            crate::game::kick(world, entity, reason);
        }
        self.0.systems(world, ctx);
    }
}

impl<G: TeamGame> TeamGame for Kicker<G> {
    fn team_of(&mut self, world: &World, conn: ConnectionId, entity: Entity) -> Team {
        self.0.team_of(world, conn, entity)
    }
}

impl<G: ShardGame> ShardGame for Kicker<G> {
    type Mig = G::Mig;

    fn capture(&self, world: &World, entity: Entity) -> G::Mig {
        self.0.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: G::Mig) -> Entity {
        self.0.restore(world, mig)
    }
}

mod fixture;
mod packed;
mod rated;
mod requests;
mod volume;

pub(crate) use fixture::*;
pub(crate) use packed::*;
pub(crate) use rated::*;
pub(crate) use volume::*;
