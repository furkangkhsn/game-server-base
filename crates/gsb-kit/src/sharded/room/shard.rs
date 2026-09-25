//! The sharding half: this room's index, wire range, neighbours, and
//! the migration/border callbacks the shard actor drives.

use bevy_ecs::prelude::{Entity, With, World};
use gsb_core::id::PlayerId;
use gsb_core::room::{Action, TickCtx};
use gsb_core::shard::{
    BorderRecord, CrossSeam, EffectOutcome, Migrating, RemoteEffect, SHARD_SERIAL_RANGE, ShardLogic,
};

use crate::common::ParkEntry;
use crate::game::{ShardGame, Wire};
use crate::identity::WireId;
use crate::sharded::room::*;
use crate::space::Partition;

impl<G: ShardGame, P: Partition<Wire<G>>> ShardLogic<World> for ShardedRoom<G, P> {
    type State = KitMig<G::Mig>;

    fn index(&self) -> usize {
        self.index
    }

    fn shard_count(&self) -> usize {
        self.partition.shard_count()
    }

    fn serial_base(&self) -> u64 {
        self.index as u64 * SHARD_SERIAL_RANGE
    }

    fn serial_range(&self) -> u64 {
        SHARD_SERIAL_RANGE
    }

    fn serial_used(&self) -> u64 {
        self.minter.used()
    }

    fn neighbors(&self) -> &[usize] {
        &self.neighbors
    }

    fn collect_migrations(
        &mut self,
        world: &mut World,
        neighbor: usize,
    ) -> Vec<Migrating<Self::State>> {
        // Every broadcast entity (the codec's marker — §8.5: whatever
        // else it carries) whose POST-step position lies in a region that
        // `neighbor` is the first hop toward (`route` — the neighbour's
        // own region, or a region beyond it: §8.4) — or, while a fight
        // holds it (crystallization), whose ANCHOR is: a held entity
        // stays on the shard of its fight whatever its region, and a
        // mover goes to it (`Crystal::anchor`); the crossing was
        // sampled at the end of this tick; the core installs them in the
        // neighbor at the next tick and despawns them here the tick
        // after — see `gsb_core::shard`'s module docs. Each entity is in
        // exactly one region, and each region has one first hop, so it is
        // reported to exactly one neighbor.
        let mut crossing: Vec<(Entity, u64)> = Vec::new();
        {
            let mut query = world.query_filtered::<(Entity, &WireId, &P::Pos), With<Marker<G>>>();
            let crystal = self.crystal.as_ref();
            for (entity, wire, pos) in query.iter(world) {
                let region = crystal
                    .and_then(|c| c.anchor(wire.get()))
                    .unwrap_or_else(|| self.partition.region_of(pos));
                if region != self.index && self.route[region] == neighbor {
                    crossing.push((entity, wire.get()));
                }
            }
        }
        crossing
            .into_iter()
            .map(|(entity, wire)| {
                // §14.2: the park record travels WITH the player state.
                // The ledger is tiny (parks are rare), so the reverse
                // lookup is a scan over it.
                let park = self
                    .park_ledger
                    .iter()
                    .find(|(_, p)| p.entity == entity)
                    .map(|(identity, p)| ShardParkRecord {
                        identity: identity.clone(),
                        player: p.player,
                        wire,
                        bot: p.bot,
                    });
                // The stable player identity travels with the entity: the
                // receiving shard keys its row under the SAME id.
                let player = self.entity_player.get(&entity).copied();
                // K1–K3: the player's input session travels too. Read,
                // not taken: a refused send leaves the player here (the
                // core rolls its row back and re-collects next tick), so
                // the entry leaves only when the move commits
                // (`on_migrate_out`).
                let input = player
                    .and_then(|p| self.input.mark(p))
                    .map(|(hwm, acked)| ShardInputRecord { hwm, acked });
                Migrating {
                    wire,
                    // What else travels is the game's (`ShardGame::capture`).
                    state: KitMig {
                        game: self.game.capture(world, entity),
                        park,
                        input,
                        // A mover carries its pin: the receiving shard
                        // holds it there (and its partner).
                        pin: self
                            .crystal
                            .as_ref()
                            .and_then(|c| c.carry(wire, self.index)),
                    },
                    player,
                }
            })
            .collect()
    }

    fn on_migrate_in(
        &mut self,
        world: &mut World,
        wire: u64,
        state: Self::State,
        player: Option<PlayerId>,
    ) {
        // The game rebuilds the entity from its captured state; the kit
        // re-stamps the wire identity it travelled with (range
        // partitioning — the sibling's minter minted it, this one only
        // re-materializes it).
        let KitMig {
            game: mig,
            park,
            input,
            pin,
        } = state;
        let entity = self.game.restore(world, mig);
        debug_assert!(
            world.entity(entity).contains::<Marker<G>>(),
            "ShardGame::restore must spawn the codec's Marker (the broadcast set)"
        );
        world.entity_mut(entity).insert(self.minter.arrival(wire));
        self.wire_entity.insert(wire, entity);
        self.entity_wire.insert(entity, wire);
        if let Some(player) = player {
            // The session's channel halves were moved with the message
            // (the core re-registers its row and binding); here the logic
            // only records the player↔entity bookkeeping so `on_leave`
            // and the next `collect_migrations` see it — under the SAME
            // stable key the sending shard used.
            self.player_entity.insert(player, entity);
            self.entity_player.insert(entity, player);
            // K1–K3: the session continues from the carried state — the
            // mark keeps the sequence rule, and an ack the source never
            // sent goes out in this shard's next private frame. Without
            // one (a sender that carried none), a fresh session.
            match input {
                Some(rec) => self.input.adopt(player, rec.hwm, rec.acked),
                None => self.input.begin(player),
            }
        }
        // Crystallization: a mover is held here with its partner (a room
        // that did not opt in ignores the pin — its region owns it).
        if let (Some(pin), Some(crystal)) = (pin, self.crystal.as_mut()) {
            crystal.arrive(wire, pin, self.index, &self.wire_entity);
        }
        if let Some(park) = park {
            // §14.2: a detached/bot-fed player's ledger record arrives
            // WITH the entity — the receiving shard now owns the park
            // (its `resume_lookup` answers, its ingest feeds the bot).
            self.park_ledger.insert(
                park.identity,
                ParkEntry {
                    player: park.player,
                    entity,
                    bot: park.bot,
                },
            );
        }
    }

    fn on_migrate_out(&mut self, world: &mut World, wire: u64) {
        if let Some(crystal) = self.crystal.as_mut() {
            crystal.leave(wire);
        }
        if let Some(entity) = self.wire_entity.remove(&wire)
            && world.get_entity(entity).is_ok()
        {
            if let Some(player) = self.entity_player.remove(&entity) {
                self.player_entity.remove(&player);
                // K3: the input session left with the player (inside
                // `KitMig`); one entry would leak here per migration.
                self.input.end(player);
            }
            self.entity_wire.remove(&entity);
            // §14.2 symmetry: the park record left with the entity (it
            // was attached to the migration state); drop it here so the
            // old shard's ledger never answers for a player it no longer
            // hosts.
            self.park_ledger.retain(|_, p| p.entity != entity);
            world.despawn(entity);
        }
    }

    fn collect_border(&self, _world: &World) -> Vec<BorderRecord<Wire<G>>> {
        // The boundary cache (rebuilt in `update`; see the field docs for
        // the one-tick-stale-with-respect-to-migrate-out note). The set is
        // what the partition exports from this region; the exchange is
        // sent whole to every neighbor and each consumer's frame filter
        // (`Partition::admits`) discards the irrelevant parts.
        self.border_cache.clone()
    }

    fn own_wires(&self, _world: &World) -> Vec<u64> {
        // The owned-wire set: `wire_entity`'s keys (kept in sync on every
        // mutation; see the field docs for why it cannot be a stale
        // cache).
        self.wire_entity.keys().copied().collect()
    }

    // -- Across the seam: the core lends its view; the kit joins it with
    //    the owned-wire table (own wins) and hands it to the game. ------

    fn ingest_seam(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        seam: &mut CrossSeam<'_, Wire<G>>,
    ) {
        crate::common::ingest_seam(
            &mut self.game,
            world,
            ctx,
            actions,
            &self.player_entity,
            &self.park_ledger,
            &mut self.input,
            &mut Seam::new(seam, &self.wire_entity, self.crystal.as_mut()),
        );
    }

    fn update_seam(&mut self, world: &mut World, ctx: &TickCtx, seam: &mut CrossSeam<'_, Wire<G>>) {
        self.step(world, ctx, Some(seam));
        crate::common::close_change_window(world);
    }

    fn apply_remote_effect(
        &mut self,
        world: &mut World,
        tick: u64,
        effect: &RemoteEffect,
        seam: &mut CrossSeam<'_, Wire<G>>,
    ) -> EffectOutcome {
        // The target is ours if the owned-wire table names a live entity
        // (an entity the game despawned this tick is still named until
        // the tick's sweep — hence the world check).
        let Some(&target) = self.wire_entity.get(&effect.target) else {
            return EffectOutcome::NoTarget;
        };
        if world.get_entity(target).is_err() {
            return EffectOutcome::NoTarget;
        }
        let mut seam = Seam::new(seam, &self.wire_entity, self.crystal.as_mut());
        let game = &mut self.game;
        let outcome = crate::common::guard_change_window(world, |w| {
            game.apply_remote_effect(w, target, effect, tick, &mut seam)
        });
        // A landed effect is a contact from across the seam (the other
        // direction of what `Seam::emit` records).
        if outcome == EffectOutcome::Applied {
            seam.contact(effect.source, effect.target);
        }
        outcome
    }
}
