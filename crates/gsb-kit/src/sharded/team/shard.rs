//! The composite's sharding half: the grid protocol delegated, the team
//! carried across a migration, and the team exchange.

use bevy_ecs::prelude::World;
use gsb_core::id::PlayerId;
use gsb_core::room::{Action, TickCtx};
use gsb_core::shard::{
    BorderRecord, CrossSeam, EffectOutcome, Migrating, RemoteEffect, ShardLogic, TeamExport,
    TeamImports,
};

use crate::game::{ShardGame, TeamGame, Wire};
use crate::sharded::team::*;
use crate::space::{Partition, Vision};

impl<G, P, V> ShardLogic<World> for ShardedTeamRoom<G, P, V>
where
    G: ShardGame + TeamGame,
    P: Partition<Wire<G>>,
    V: Vision,
{
    type State = TeamMig<G::Mig>;

    fn index(&self) -> usize {
        self.inner.index()
    }

    fn shard_count(&self) -> usize {
        self.inner.shard_count()
    }

    fn serial_capacity(&self) -> u64 {
        self.inner.serial_capacity()
    }

    fn serial_used(&self) -> u64 {
        self.inner.serial_used()
    }

    fn neighbors(&self) -> &[usize] {
        self.inner.neighbors()
    }

    /// The grid's crossings, each with the entity's team (read before the
    /// core despawns the copy next tick).
    fn collect_migrations(
        &mut self,
        world: &mut World,
        neighbor: usize,
    ) -> Vec<Migrating<Self::State>> {
        let moves = self.inner.collect_migrations(world, neighbor);
        moves
            .into_iter()
            .map(|m| {
                let team = self
                    .inner
                    .wire_entity
                    .get(&m.wire)
                    .and_then(|&e| Self::team_of_entity(world, e));
                Migrating {
                    wire: m.wire,
                    state: TeamMig { kit: m.state, team },
                    player: m.player,
                }
            })
            .collect()
    }

    /// Install the entity through the grid half, give it its team back,
    /// and drop an arriving player's baseline: this shard's team view is
    /// new to its session (delta mode: the one-shot full).
    fn on_migrate_in(
        &mut self,
        world: &mut World,
        wire: u64,
        state: Self::State,
        player: Option<PlayerId>,
    ) {
        let TeamMig { kit, team } = state;
        self.inner.on_migrate_in(world, wire, kit, player);
        if let (Some(team), Some(&entity)) = (team, self.inner.wire_entity.get(&wire)) {
            world.entity_mut(entity).insert(TeamMember(team));
        }
        if let Some(player) = player {
            self.baselines.forget(player);
        }
    }

    fn on_migrate_out(&mut self, world: &mut World, wire: u64) {
        if let Some(player) = self
            .inner
            .wire_entity
            .get(&wire)
            .and_then(|e| self.inner.entity_player.get(e))
        {
            self.baselines.forget(*player);
        }
        self.inner.on_migrate_out(world, wire);
    }

    fn collect_border(&self, world: &World) -> Vec<BorderRecord<Wire<G>>> {
        self.inner.collect_border(world)
    }

    fn own_wires(&self, world: &World) -> Vec<u64> {
        self.inner.own_wires(world)
    }

    fn ingest_seam(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        seam: &mut CrossSeam<'_, Wire<G>>,
    ) {
        self.inner.ingest_seam(world, ctx, actions, seam);
    }

    fn update_seam(&mut self, world: &mut World, ctx: &TickCtx, seam: &mut CrossSeam<'_, Wire<G>>) {
        self.step += 1;
        self.tick = ctx.tick;
        self.inner.update_seam(world, ctx, seam);
    }

    fn apply_remote_effect(
        &mut self,
        world: &mut World,
        tick: u64,
        effect: &RemoteEffect,
        seam: &mut CrossSeam<'_, Wire<G>>,
    ) -> EffectOutcome {
        self.inner.apply_remote_effect(world, tick, effect, seam)
    }

    /// The TEAMS phase: every viewed team's content from own + lent +
    /// imported records, and this shard's export (`content`).
    fn team_exchange(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        borrowed: &[BorderRecord<Wire<G>>],
        imported: &TeamImports,
    ) -> Option<TeamExport> {
        Some(self.exchange(world, ctx.tick, borrowed, imported))
    }
}
