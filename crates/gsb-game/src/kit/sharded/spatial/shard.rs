//! The composite's sharding half.

use bevy_ecs::prelude::World;
use gsb_core::id::PlayerId;
use gsb_core::shard::{BorderRecord, Migrating, ShardLogic};

use crate::kit::seam::StripPos;
use crate::kit::sharded::spatial::DemoShard;
use crate::kit::sharded::*;

impl ShardLogic<World> for ShardedSpatialRoom {
    type State = <DemoShard as ShardLogic<World>>::State;

    fn index(&self) -> usize {
        self.inner.index()
    }

    fn shard_count(&self) -> usize {
        self.inner.shard_count()
    }

    fn serial_base(&self) -> u64 {
        self.inner.serial_base()
    }

    fn serial_range(&self) -> u64 {
        self.inner.serial_range()
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
            self.book.members.remove(&entity);
            let cell = self.book.last_cell.get(&entity).copied();
            if let Some(cell) = cell {
                self.book.pending_removals.push((entity, wire, cell));
            }
            if let Some(player) = self.inner.entity_player.get(&entity).copied() {
                self.conn_view.remove(&player);
            }
        }
        self.inner.on_migrate_out(world, wire);
    }

    fn collect_border(&self, world: &World) -> Vec<BorderRecord<StripPos>> {
        self.inner.collect_border(world)
    }

    fn own_wires(&self, world: &World) -> Vec<u64> {
        self.inner.own_wires(world)
    }
}
