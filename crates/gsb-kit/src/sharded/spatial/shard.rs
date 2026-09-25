//! The composite's sharding half.

use bevy_ecs::prelude::World;
use gsb_core::id::PlayerId;
use gsb_core::room::{Action, TickCtx};
use gsb_core::shard::{
    BorderRecord, CrossSeam, EffectOutcome, Migrating, RemoteEffect, ShardLogic,
};

use crate::game::{ShardGame, Wire};
use crate::sharded::*;
use crate::space::{CellSpace, Partition};

impl<G, P, S> ShardLogic<World> for ShardedSpatialRoom<G, P, S>
where
    G: ShardGame,
    P: Partition<Wire<G>>,
    S: CellSpace<Wire<G>>,
{
    type State = KitMig<G::Mig>;

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

    fn collect_migrations(
        &mut self,
        world: &mut World,
        neighbor: usize,
    ) -> Vec<Migrating<Self::State>> {
        self.inner.collect_migrations(world, neighbor)
    }

    fn on_migrate_in(
        &mut self,
        world: &mut World,
        wire: u64,
        state: Self::State,
        player: Option<PlayerId>,
    ) {
        self.inner.on_migrate_in(world, wire, state, player);
        let Some(player) = player else { return };
        // Member bookkeeping for the receiving cell's birth arithmetic…
        if let Some(&entity) = self.inner.player_entity.get(&player) {
            self.book.members.insert(entity);
        }
        // …and the FRESH-MEMBER RULE (module docs, "Migration
        // correctness"): the arrival has no baseline for its new cell's
        // view — clear it so the next private frame is the one-shot full
        // of the local world.
        self.conn_view.remove(&player);
    }

    fn on_migrate_out(&mut self, world: &mut World, wire: u64) {
        // Capture BEFORE the delegate despawns: the leaver's last cell
        // (for the parked removal — the seam cell's delta carries the
        // exit) and its stable player (for the baseline drop).
        if let Some(entity) = self.inner.wire_entity.get(&wire).copied() {
            // A migrating NPC was never a member: the flag keeps the
            // cell's member arithmetic exact.
            let member = self.book.members.remove(&entity);
            if self.book.last_cell.contains_key(&entity) {
                self.book.pending_removals.push((entity, member));
            }
            if let Some(player) = self.inner.entity_player.get(&entity).copied() {
                self.conn_view.remove(&player);
            }
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
        // `update`'s two halves, the grid half through the seam.
        self.inner.step(world, ctx, Some(seam));
        self.spatial_step(world, ctx);
    }

    fn apply_remote_effect(
        &mut self,
        world: &mut World,
        tick: u64,
        effect: &RemoteEffect,
        seam: &mut CrossSeam<'_, Wire<G>>,
    ) -> EffectOutcome {
        // An effect's writes and despawns land before `update`: the dirty
        // pass and the removed-buffer sweep of this tick pick them up
        // like any game write.
        self.inner.apply_remote_effect(world, tick, effect, seam)
    }
}
