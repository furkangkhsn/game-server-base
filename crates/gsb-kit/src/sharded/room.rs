//! The whole-world sharded room: N shard actors over one map, every
//! shard broadcasting its own slice to everyone in it.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, With, Without, World};
use gsb_core::id::PlayerId;
use gsb_core::room::TickCtx;
use gsb_core::shard::{BorderRecord, CrossSeam};

use crate::codec::RecordCodec;
use crate::common::{InputSeq, ParkEntry, ParkPolicy};
use crate::game::{Game, ShardGame, Wire};
use crate::identity::{Minter, WireId};
use crate::sharded::crystal::Crystal;
use crate::sharded::departing::Departures;
use crate::sharded::*;
use crate::space::Partition;

mod join;
mod logic;
mod policy;
mod shard;

/// The game's broadcast marker (the codec's `Marker`).
type Marker<G> = <<G as Game>::Codec as RecordCodec>::Marker;
/// The game's record query (the codec's `Query`).
type RecordQuery<G> = <<G as Game>::Codec as RecordCodec>::Query;

/// The sharded-room shard logic: one region of the partition (see
/// module docs).
pub struct ShardedRoom<G: ShardGame, P: Partition<Wire<G>>> {
    /// The game (its hooks, its codec and its own state — the demo's
    /// system stack, spawn map, economy handle).
    pub(in crate::sharded) game: G,
    /// The map partition (shared by all shards — the factory builds
    /// every shard with the same one).
    pub(in crate::sharded) partition: P,
    /// This shard's region index.
    pub(in crate::sharded) index: usize,
    /// The regions bordering this one ([`Partition::neighbors`], cached:
    /// the core reads them as a slice every tick and sends border and
    /// migration messages to exactly these).
    pub(in crate::sharded) neighbors: Vec<usize>,
    /// `route[region]`: the neighbour an entity whose position lies in
    /// `region` is handed to — the first hop of a shortest path over the
    /// neighbour graph (itself for a neighbour's region; this shard's
    /// own index for its own region). A crossing into a region that is
    /// not a neighbour's (a move through a grid corner, a jump) travels
    /// hop by hop, one tick per hop (§8.4).
    pub(in crate::sharded) route: Vec<usize>,
    /// Player → entity (this shard's players; Faz 2: keyed by the STABLE
    /// player identity, which survives resume AND migration unchanged).
    pub(in crate::sharded) player_entity: HashMap<PlayerId, Entity>,
    /// The disconnect-park policy (see `crate::common`; RECONNECT §3).
    pub(in crate::sharded) park: ParkPolicy,
    /// The park ledger of THIS shard's parked players — the same ledger
    /// and hook bodies as every single-world room (`common/park.rs`);
    /// §14.2: an entry leaves with its entity's migration (inside
    /// [`KitMig`]) and is filed again on the receiving shard, so a
    /// detached entity crossing a seam is never stranded on the old one.
    pub(in crate::sharded) park_ledger: HashMap<String, ParkEntry>,
    /// Entity → owning player (only entities owned by a player).
    pub(in crate::sharded) entity_player: HashMap<Entity, PlayerId>,
    /// Wire id → entity: every entity this shard currently owns, kept in
    /// sync on every mutation (join/leave/migrate-in/out/stamp, and a
    /// despawn by game code — swept from the world's removed buffer,
    /// §8.2). Its key set is the owned-wire set `own_wires` reports
    /// (`own_wires` takes `&World`, it cannot query). Accuracy matters
    /// for the core's duplicate filter: a stale entry for an entity that
    /// just migrated out would hide the neighbor's (now-correct) record
    /// of it, dropping it from this shard's view for a tick.
    pub(in crate::sharded) wire_entity: HashMap<u64, Entity>,
    /// Entity → wire id, the reverse of `wire_entity`: a despawned
    /// entity's components are gone, so this is how the sweep finds the
    /// wire id of an entity the game despawned.
    pub(in crate::sharded) entity_wire: HashMap<Entity, u64>,
    /// This shard's identity counter, interleaved with its siblings'
    /// (`(n − 1) · shard_count + index + 1` — an interleaved [`Minter`],
    /// the only construction path of [`WireId`]). BOTH identity spaces
    /// draw from this one counter — wire ids AND stable player ids — so
    /// the core's exhaustion guard (`serial_used`) stays exact over
    /// everything the counter backs.
    pub(in crate::sharded) minter: Minter,
    /// This shard's boundary records (module docs, "Visibility model"),
    /// rebuilt at the end of `update` (positions change in the game's
    /// systems). `collect_border` takes `&World` (it cannot query), so it
    /// returns a clone of this cache. It is one tick stale with respect
    /// to the phase-4 migrate-out despawns (an entity that just crossed
    /// out is still exported) — harmless: the receiving shard owns it
    /// now and its own-wires filter drops the stale copy (own wins).
    pub(in crate::sharded) border_cache: Vec<BorderRecord<Wire<G>>>,
    /// The wire content of the last emitted snapshot (single group,
    /// `GroupKey = ()`): `wire id → wire value`.
    pub(in crate::sharded) last: HashMap<u64, Wire<G>>,
    /// Entity records encoded during the most recent broadcast phase.
    pub(in crate::sharded) encoded: u64,
    /// Per-player input sequence state (strategy-independent; see
    /// `crate::common::emit_private`) of THIS shard's players. A
    /// player's entry moves with it: carried in [`KitMig`]
    /// ([`ShardInputRecord`]), dropped here when the move commits,
    /// continued by the receiving shard.
    pub(in crate::sharded) input: InputSeq,
    /// Crystallization (`None` unless the room opted in —
    /// [`Self::with_crystallize`]): the fight table and the pins that
    /// decouple a fighting entity's owner from its region.
    pub(in crate::sharded) crystal: Option<Crystal>,
    /// The entities this shard hands on: their captured records, and
    /// the doomed copies the game's hooks of the next tick do not see
    /// (`crate::sharded::departing`).
    pub(in crate::sharded) departures: Departures<Wire<G>>,
}

impl<G: ShardGame, P: Partition<Wire<G>>> ShardedRoom<G, P> {
    /// Build shard `index` of a room running `game` over `partition`
    /// (every shard of a room gets the same partition).
    pub fn with_game(game: G, partition: P, index: usize) -> Self {
        let neighbors = partition.neighbors(index);
        let route = first_hops(index, partition.shard_count(), |i| partition.neighbors(i));
        let minter = Minter::interleaved(index, partition.shard_count());
        Self {
            game,
            partition,
            index,
            neighbors,
            route,
            player_entity: HashMap::new(),
            park: ParkPolicy::default(),
            park_ledger: HashMap::new(),
            entity_player: HashMap::new(),
            wire_entity: HashMap::new(),
            entity_wire: HashMap::new(),
            minter,
            border_cache: Vec::new(),
            last: HashMap::new(),
            encoded: 0,
            input: InputSeq::default(),
            crystal: None,
            departures: Departures::default(),
        }
    }

    /// The game this shard runs.
    pub fn game(&self) -> &G {
        &self.game
    }

    /// The game this shard runs, for configuration after construction
    /// (e.g. attaching a service handle the game's requests delegate to).
    pub fn game_mut(&mut self) -> &mut G {
        &mut self.game
    }

    /// Mint the next STABLE player identity (Faz 2): interleaved like
    /// the wire ids — drawn from the SAME counter (see the `minter` field
    /// docs), so two shards never mint the same player.
    fn mint_player(&mut self) -> PlayerId {
        PlayerId(self.minter.next_serial())
    }

    /// The tick body minus the change-window close: the game's systems
    /// (through its seam hook when the actor lends a seam — the sharded
    /// path; the plain hook otherwise), shard-aware orphan stamping and
    /// the border-cache rebuild. The spatial composite runs this and then
    /// its own dirty pass, which must still see the tick's writes.
    pub(in crate::sharded) fn step(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        mut seam: Option<&mut CrossSeam<'_, Wire<G>>>,
    ) {
        match seam.as_deref_mut() {
            Some(cross) => {
                let own = &self.wire_entity;
                self.departures.hide(world, own, cross);
                let mut seam = Seam::new(cross, own, &self.departures, self.crystal.as_mut());
                let game = &mut self.game;
                crate::common::guard_change_window(world, |w| game.systems_seam(w, ctx, &mut seam));
                // The game's hooks of the tick are over: the doomed copies
                // are back for the passes below (and the border export).
                self.departures.show(world);
            }
            None => crate::common::systems(&mut self.game, world, ctx),
        }

        // Entities despawned since the last close that no hook of ours
        // despawned (game code — an NPC dying; §8.2): forget their wire
        // ids. The leave and migrate-out paths clean their own entries,
        // so for them this finds nothing. Read before the tick's
        // change-window close empties the buffer.
        for entity in world.removed::<WireId>() {
            if let Some(wire) = self.entity_wire.remove(&entity) {
                self.wire_entity.remove(&wire);
                self.entity_player.remove(&entity);
                self.departures.forget(wire);
            }
        }

        // Orphan stamping, shard-aware (the broadcast set stays
        // structural — "has the codec's marker" — like the other rooms):
        // entities with the marker but no `WireId` get the next value
        // of THIS SHARD'S COUNTER (a plain room counter would draw
        // values a sibling also draws and break the uniqueness
        // invariant).
        let orphans: Vec<Entity> = world
            .query_filtered::<Entity, (With<Marker<G>>, Without<WireId>)>()
            .iter(world)
            .collect();
        for entity in orphans {
            let wire = self.minter.mint();
            world.entity_mut(entity).insert(wire);
            self.wire_entity.insert(wire.get(), entity);
            self.entity_wire.insert(entity, wire.get());
        }

        // Crystallization's pass (only through the seam: it needs the
        // lent view), with the tick's final positions and owned wires —
        // before the migrate phase reads the pins.
        if let (Some(crystal), Some(cross)) = (self.crystal.as_mut(), seam) {
            crystal.evaluate(
                world,
                ctx.tick,
                self.index,
                &self.partition,
                cross,
                &self.wire_entity,
            );
        }

        // Rebuild the border cache (positions just changed in the game's
        // systems; `collect_border` cannot query — it takes `&World`).
        self.border_cache.clear();
        let codec = self.game.codec();
        let mut query =
            world.query_filtered::<(&WireId, RecordQuery<G>, &P::Pos), With<Marker<G>>>();
        for (wire, item, pos) in query.iter(world) {
            if self.partition.exports(self.index, pos) {
                let state = codec.wire(item);
                // Debug builds: the wire must agree with the position the
                // way the receivers' frame filter assumes (the preset: one
                // unit — `Planar`'s contract, KIT-ARCHITECTURE §10, F3).
                self.partition.debug_check_wire(pos, &state);
                self.border_cache.push(BorderRecord {
                    wire: wire.get(),
                    state,
                });
            }
        }
    }

    /// Every broadcastable entity of this shard as `(wire id, wire
    /// value)`, in query order.
    fn own_records(&self, world: &mut World) -> Vec<(u64, Wire<G>)> {
        let codec = self.game.codec();
        let mut query = world.query_filtered::<(&WireId, RecordQuery<G>), With<Marker<G>>>();
        query
            .iter(world)
            .map(|(wire, item)| (wire.get(), codec.wire(item)))
            .collect()
    }
}
