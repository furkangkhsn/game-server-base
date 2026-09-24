//! The sharding half: this room's index, wire range, neighbours, and
//! the migration/border callbacks the shard actor drives.

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::PlayerId;
use gsb_core::shard::{BorderRecord, Migrating, SHARD_SERIAL_RANGE, ShardLogic};

use crate::kit::identity::WireId;
use crate::kit::seam;
use crate::kit::seam::{MoveTarget, Position, Speed};
use crate::kit::sharded::*;

impl ShardLogic<World> for ShardedRoom {
    type State = ShardedRoomState;

    fn index(&self) -> usize {
        self.index
    }

    fn shard_count(&self) -> usize {
        self.shard_count
    }

    fn serial_base(&self) -> u64 {
        self.index as u64 * SHARD_SERIAL_RANGE
    }

    fn serial_range(&self) -> u64 {
        SHARD_SERIAL_RANGE
    }

    fn serial_used(&self) -> u64 {
        self.serial_used
    }

    fn neighbors(&self) -> &[usize] {
        &self.neighbors
    }

    fn collect_migrations(
        &mut self,
        world: &mut World,
        neighbor: usize,
    ) -> Vec<Migrating<Self::State>> {
        // Entities whose POST-step position lies in `neighbor`'s region
        // (the crossing was sampled at the end of this tick; the core
        // installs them in the neighbor at the next tick and despawns
        // them here the tick after — see `gsb_core::shard`'s module
        // docs). Each entity is in exactly one region, so it is reported
        // to exactly one neighbor.
        let mut out: Vec<Migrating<Self::State>> = Vec::new();
        let mut query = world.query::<(Entity, &WireId, &Position, &Speed, Option<&MoveTarget>)>();
        for (entity, wire, pos, speed, target) in query.iter(world) {
            if self.region_of(*pos) == neighbor {
                // §14.2: the park record travels WITH the player state.
                // The ledger is tiny (parks are rare), so the reverse
                // lookup is a scan over it.
                let park = self
                    .park_ledger
                    .values()
                    .find(|p| p.wire == wire.get())
                    .cloned();
                out.push(Migrating {
                    wire: wire.get(),
                    state: ShardedRoomState {
                        pos: *pos,
                        speed: speed.0,
                        target: target.copied(),
                        park,
                    },
                    // The stable player identity travels with the entity:
                    // the receiving shard keys its row under the SAME id.
                    player: self.entity_player.get(&entity).copied(),
                });
            }
        }
        out
    }

    fn on_migrate_in(
        &mut self,
        world: &mut World,
        wire: u64,
        state: Self::State,
        player: Option<PlayerId>,
    ) {
        // Reconstruct the entity from its full state, keeping its wire
        // identity (the id travels with the state — range partitioning).
        let entity = seam::restore_migrant(
            world,
            WireId::new(wire),
            state.pos,
            state.speed,
            state.target,
        );
        self.wire_entity.insert(wire, entity);
        self.own_wires.insert(wire);
        if let Some(player) = player {
            // The session's channel halves were moved with the message
            // (the core re-registers its row and binding); here the logic
            // only records the player↔entity bookkeeping so `on_leave`
            // and the next `collect_migrations` see it — under the SAME
            // stable key the sending shard used.
            self.player_entity.insert(player, entity);
            self.entity_player.insert(entity, player);
        }
        if let Some(park) = state.park {
            // §14.2: a detached/bot-fed player's ledger record arrives
            // WITH the entity — the receiving shard now owns the park
            // (its `resume_lookup` answers, its ingest feeds the bot).
            self.park_ledger.insert(park.identity.clone(), park);
        }
    }

    fn on_migrate_out(&mut self, world: &mut World, wire: u64) {
        if let Some(entity) = self.wire_entity.remove(&wire)
            && world.get_entity(entity).is_ok()
        {
            if let Some(player) = self.entity_player.remove(&entity) {
                self.player_entity.remove(&player);
            }
            self.own_wires.remove(&wire);
            // §14.2 symmetry: the park record left with the entity (it
            // was attached to the migration state); drop it here so the
            // old shard's ledger never answers for a player it no longer
            // hosts.
            self.park_ledger.retain(|_, p| p.wire != wire);
            world.despawn(entity);
        }
    }

    fn collect_border(&self, _world: &World) -> Vec<BorderRecord<StripPos>> {
        // The boundary cache (rebuilt in `update`; see the field docs for
        // the one-tick-stale-with-respect-to-migrate-out note). The set is
        // this shard's entities within `border` of any edge of the region
        // rectangle; the exchange is sent whole to every neighbor and the
        // consumer's frame filter discards the irrelevant parts.
        self.border_cache.clone()
    }

    fn own_wires(&self, _world: &World) -> Vec<u64> {
        // The owned-wire set (kept in sync on every mutation; see the
        // field docs for why it cannot be a stale cache).
        self.own_wires.iter().copied().collect()
    }
}
