//! [`ShardedRoom`]: the shard-level [`ShardLogic`] for the demo game.
//!
//! ## What it is
//!
//! The same game as the other four rooms (same components, movement
//! system, wire format, spawn distribution — see [`crate::common`]), but
//! the world is partitioned into a **grid of shards**: a room of
//! `shard_count` actors, each owning its rectangular region of the map.
//! The core machinery (`gsb_core::shard`) runs the shard protocol
//! (migration, border exchange, range-partitioned wire ids); this module
//! supplies only the game knowledge:
//!
//! - the **region** of a position (grid cell → shard index),
//! - the **neighbor** topology (4-neighborhood of the grid),
//! - the **migration state** ([`ShardedRoomState`]: position, speed,
//!   move target — everything the entity carries),
//! - the **border** export/import (boundary visibility, one quarter-cell
//!   margin on each side),
//! - the **snapshot** (the shard's own world + the borrowed boundary
//!   records, one group — `GroupKey = ()`).
//!
//! ## Visibility model (what a player sees)
//!
//! A player sees **its shard's whole region plus a boundary margin**: the
//! own region (a `1/shard_count` slice of the map, in a grid cell) and the
//! neighboring shards' boundary entities within [`border`] of the shared
//! edge. This is distance-limited visibility — the class of game sharding
//! serves (a single continuous world where far entities are irrelevant).
//! A player at the seam sees across it (the borrowed records); a player
//! deep in its region does not see the far shards. The margin is
//! deliberately small (a quarter cell) so the borrowed set — and the
//! encoding cost it adds to every shard's snapshot — stays bounded by the
//! boundary, not the whole shard.
//!
//! ## Wire identity (range partitioning)
//!
//! Shard `i` mints ids from `[i * SHARD_SERIAL_RANGE, (i+1) *
//! SHARD_SERIAL_RANGE)`. A migrated entity **keeps its id** (it travels
//! with its state), so the id is stable across migrations and the ranges
//! being disjoint means no two shards ever mint the same id — the
//! identity invariant holds shard-locally and across the room. See
//! `gsb_core::shard`'s module docs for the full protocol.
//!
//! ## Group key
//!
//! `GroupKey = ()`: one snapshot group per shard, shared by every
//! connection in the shard. The sharded visibility is the region +
//! margin (above), not a finer per-cell block; within a shard everyone
//! sees the same bytes.
//!
//! ## The spatial composite ([`ShardedSpatialRoom`] — ROADMAP Faz B)
//!
//! [`ShardedSpatialRoom`] is the `sharded × spatial` selection: the SAME
//! grid topology, migration protocol and border seam, but each shard's
//! broadcast phase groups its connections by **spatial cell**
//! (`GroupKey = Cell`, sized like [`crate::aoi::AoiRoom`]'s from the
//! config's `aoi_cell_size`) instead of one whole-shard group. The cell
//! encoding/delta engine itself is NOT duplicated: the shared
//! [`crate::common::CellBook`] / [`crate::common::CellPieces`] machinery
//! drives both rooms. What this module adds on top is exactly the part a
//! single world cannot have — the borrowed border strip — and it is the
//! load-bearing subtlety of the whole composite:
//!
//! ### THE borrowed-strip × delta-ledger subtlety (why a naive port decays)
//!
//! The borrowed records arrive at the broadcast phase FULLY REPLACED every
//! tick (the receiver-side view of the seq-stamped border-delta protocol
//! is kept wholesale by the core actor; quarantine aside, the flattened
//! slice always names every currently-borrowed entity and its CURRENT
//! truncated position). Diffing that slice against "the world as of last
//! tick" — the way own entities are diffed through bevy's change
//! detection — would therefore flag EVERY borrowed record as written on
//! EVERY tick: every cell touching the strip would go dirty tick after
//! tick, the deltas would degenerate toward full re-carries, and the
//! entire byte economy of the cell encoding would evaporate exactly where
//! shards touch (the densest places — players cluster near points of
//! interest, and POIs sit near seams by design).
//!
//! So the borrowed set participates in the delta bookkeeping through its
//! OWN ledger: the room keeps the PREVIOUS tick's borrowed view
//! (`prev_borrowed`: wire → wire position) and diffs the NEW view against
//! THAT — entered (absent before), exited (gone now), moved (position
//! changed), silent (identical wire position ⇒ NO change entry at all).
//! Only the diff lands in the shared [`crate::common::CellBook`] change
//! lists, so a static strip costs nothing beyond the comparison itself,
//! and a moving boundary entity produces exactly one upsert (plus an exit
//! when it changes cell) — the same shape an own-entity mover produces.
//!
//! Why ONE flat ledger instead of per-neighbor ledgers: the wire ids are
//! range-partitioned PER SHARD, so a borrowed id identifies exactly one
//! neighbor for the room's whole lifetime — the union of per-neighbor
//! diffs is mathematically the flat diff, minus a second level of maps.
//! (The core actor already merges the per-neighbor views into the sorted
//! slice this room receives; it also drops a neighbor's stale copy of an
//! entity that just migrated IN — own wins — which composes cleanly: the
//! ledger simply never saw that id, so no spurious exit is ever shipped
//! for it.)
//!
//! Two protocol events fall out of the ledger correctly BY CONSTRUCTION,
//! not by extra code: a QUARANTINED neighbor (a rejected border delta —
//! `stale_until_full`) vanishes from the flattened slice, which the
//! ledger renders as exits, and the healing Full re-renders as entries —
//! exactly what clients should see; and a crossing entity appears as an
//! exit on the leaving side's strip and an entry on the arriving side's,
//! with the own-wins filter swallowing the double view on the transition
//! tick (the accepted one-tick alignment blink, never a duplicate).
//!
//! ### Ordering: why the occupancy/birth roll cannot live in `update`
//!
//! Own entities are bucketed during `update` (the bevy dirty pass); the
//! borrowed diff can only run later — the core hands the strip to the
//! logic at the broadcast phase, after `update`. The shared engine's
//! appeared/exited/birth flags must reflect the FINAL content of the
//! tick, so the composite defers [`crate::common::CellBook::roll`] until
//! the first broadcast-phase call integrates the strip (a once-per-tick
//! guard; every snapshot/keepalive/private call is preceded by it). With
//! no connections there is no broadcast and no roll — and no client to
//! tell; the next broadcast's roll re-baselines from the final buckets
//! and any fresh group opens with a full packet regardless of flags.
//!
//! ### Migration correctness (fresh-member rule)
//!
//! A player migrating INTO a shard is dropped into a cell whose group may
//! be long-established — a delta would carry nothing to build their view
//! from. [`ShardLogic::on_migrate_in`] therefore clears the arrival's
//! view baseline, so the arrival's next `private` frame is the one-shot
//! FULL of their new 3×3 (skipped only when the group's own packet that
//! batch was already a full) — the same contract a late joiner gets on
//! the single-world AOI room. Symmetrically, a migrate-out removes the
//! leaver's baseline and parks the despawn removal (despawns are not
//! component writes) so the seam cell's delta carries the exit.

use std::collections::{HashMap, HashSet};

use bevy_ecs::prelude::{Entity, World};
use bytes::BufMut;
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, TickCtx};
use gsb_core::rpc::RequestDecision;
use gsb_core::shard::{BorderRecord, Migrating, ShardLogic, SHARD_SERIAL_RANGE};
use gsb_ecs::SystemRunner;
use prost::encoding::varint::encode_varint;
use prost::Message;

use crate::common::{
    assemble_group_packet, cell_of, Cell, CellBook, CellPieces,
};
use crate::components::{DEFAULT_SPEED, MoveTarget, Position, Speed, WireId};
use crate::economy::EconomyService;
use crate::op;
use crate::room::spawn_pos;

/// The demo's visibility-strip payload ([`GameLogic::Strip`]): the
/// entity's TRUNCATED position — exactly the content the core-fixed
/// boundary record carried before generalization, so this round changes
/// no wire bytes. A game needing more across the seam extends THIS type
/// (velocity, facing, hp snapshot); the core never learns about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripPos {
    pub x: i32,
    pub y: i32,
}

/// The full state of a migrating entity (everything the entity carries in
/// its components — position, speed, and the pending move target, if any).
/// Opaque to the core; reconstructed into components on
/// [`ShardedRoom::on_migrate_in`].
///
/// The `park` field is the RECONNECT §14.2 rule in action: a parked (or
/// bot-fed) player's ledger record is part of the migrating PLAYER state,
/// not a side table — an entity that crosses a seam while detached
/// carries its park record along, so the receiving shard's ledger answers
/// the resume and keeps feeding the bot.
#[derive(Debug, Clone)]
pub struct ShardedRoomState {
    pub pos: Position,
    pub speed: f32,
    pub target: Option<MoveTarget>,
    /// The entity's park record, if it is parked or bot-fed (`None` for
    /// every live session and every NPC).
    pub park: Option<ShardParkRecord>,
}

/// One shard-side park-ledger entry / migration-carried record. Keyed by
/// identity in the ledger; carried inside [`ShardedRoomState`] because
/// that is what survives migrations.
#[derive(Debug, Clone)]
pub struct ShardParkRecord {
    /// The resume key of the parked session.
    pub identity: String,
    /// The parked session's STABLE player identity (Faz 2): what
    /// `resume_lookup` answers (the core finds its row by one lookup)
    /// and what the bot synthesizes input under. Travels with the record
    /// across migrations, so the identity is stable end to end.
    pub player: PlayerId,
    /// The parked entity's wire id — stable across migrations, so it is
    /// what the bot resolves through this shard's wire table.
    pub wire: u64,
    /// Latched at AI-handover expiry: the bot owns the entity.
    pub bot: bool,
}

/// The grid shape for `shard_count` shards: `rows` = the largest divisor
/// of `shard_count` that is ≤ √N, `cols` = N / rows — the shape closest
/// to a square (balanced region sizes). `shard_count` must be 1..=256.
///
/// Examples: 1→1×1, 2→1×2, 4→2×2, 6→2×3, 8→2×4, 12→3×4, 16→4×4,
/// 25→5×5.
pub fn grid_shape(shard_count: usize) -> (usize, usize) {
    assert!(
        (1..=256).contains(&shard_count),
        "shard_count must be 1..=256 (grid topology), got {shard_count}"
    );
    let sqrt = (shard_count as f64).sqrt().floor() as usize;
    for d in (1..=sqrt).rev() {
        if shard_count.is_multiple_of(d) {
            return (d, shard_count / d);
        }
    }
    (1, shard_count) // unreachable: d=1 always divides
}

/// The shard index owning the position `(x, y)` on a map of half-size
/// `half`, partitioned into `shard_count` shards in a `grid_shape` grid.
/// The map spans `[-half, half]²`; each column spans `2*half/cols` in x,
/// each row `2*half/rows` in y. Positions are clamped into the grid (the
/// map has no walls, but a stray coordinate must still own exactly one
/// shard — the "exactly one owner" invariant).
pub fn shard_at(x: f32, y: f32, half: f32, shard_count: usize) -> usize {
    let (rows, cols) = grid_shape(shard_count);
    let cell_w = 2.0 * half / cols as f32;
    let cell_h = 2.0 * half / rows as f32;
    let col = (((x + half) / cell_w).floor() as i32).clamp(0, (cols - 1) as i32);
    let row = (((y + half) / cell_h).floor() as i32).clamp(0, (rows - 1) as i32);
    row as usize * cols + col as usize
}

/// The sharded-room shard logic: one shard of the grid (see module docs).
pub struct ShardedRoom {
    runner: SystemRunner,
    /// This shard's index in the grid (row-major: `row * cols + col`).
    index: usize,
    shard_count: usize,
    /// The grid's column count (used by [`Self::rect`]).
    cols: usize,
    /// Half-size of the square map (shared by all shards — the factory
    /// builds every shard with the same `spawn_half`).
    half: f32,
    cell_w: f32,
    cell_h: f32,
    /// Boundary margin (module docs, "Visibility model"): a quarter of the
    /// smallest cell dimension. Entities within this of a shared edge are
    /// exported to the neighbor and appear in the neighbor's snapshots.
    border: f32,
    /// The 4-neighborhood of this shard in the grid (indices), in stable
    /// order (west, east, north, south — the core sends border/migrate to
    /// exactly these).
    neighbors: Vec<usize>,
    /// Player → entity (this shard's players; Faz 2: keyed by the STABLE
    /// player identity, which survives resume AND migration unchanged).
    player_entity: HashMap<PlayerId, Entity>,
    /// The disconnect-park policy (see `crate::common`; RECONNECT §3).
    park: crate::common::ParkPolicy,
    /// The park ledger of THIS shard's parked players (§4: it lives in
    /// the logic; §14.2: records travel with migrations inside
    /// [`ShardedRoomState`], so a detached entity crossing a seam is
    /// parked on the receiving shard, never stranded on the old one).
    park_ledger: HashMap<String, ShardParkRecord>,
    /// Entity → owning player (only entities owned by a player).
    entity_player: HashMap<Entity, PlayerId>,
    /// Wire id → entity (every entity, for migrate-out despawn).
    wire_entity: HashMap<u64, Entity>,
    /// How many identities this shard has minted (the range is
    /// `index * SHARD_SERIAL_RANGE + serial_used`). BOTH identity spaces
    /// draw from this one counter — wire ids AND stable player ids — so
    /// the core's range-exhaustion guard stays exact over everything the
    /// range backs.
    serial_used: u64,
    /// The wire ids this shard currently owns, kept in sync on every
    /// mutation (join/leave/migrate-in/out). `own_wires` takes `&World`
    /// (it cannot query), so it reads this set instead. Accuracy matters
    /// for the core's duplicate filter: a stale entry for an entity that
    /// just migrated out would hide the neighbor's (now-correct) record
    /// of it, dropping it from this shard's view for a tick.
    own_wires: HashSet<u64>,
    /// This shard's boundary records (module docs, "Visibility model"),
    /// rebuilt at the end of `update` (positions change in the movement
    /// system). `collect_border` takes `&World` (it cannot query), so it
    /// returns a clone of this cache. It is one tick stale with respect
    /// to the phase-4 migrate-out despawns (an entity that just crossed
    /// out is still exported) — harmless: the receiving shard owns it
    /// now and its own-wires filter drops the stale copy (own wins).
    border_cache: Vec<BorderRecord<StripPos>>,
    /// The wire content of the last emitted snapshot (single group,
    /// `GroupKey = ()`): `wire → (x, y)` (truncated).
    last: HashMap<u64, (i32, i32)>,
    /// Entity records encoded during the most recent broadcast phase.
    encoded: u64,
    /// Per-player input sequence state (strategy-independent; see
    /// `crate::common::ingest` / `emit_private`). The session stays bound to
    /// this shard even if its entity migrates (its input is routed
    /// through this shard's room), so the session lives here.
    input: HashMap<PlayerId, crate::common::InputState>,
    /// The economy service handle (the RPC pattern's external-I/O half on
    /// the SHARDED path — Faz 3; see `crate::economy`); `None` = this
    /// shard answers `ECONOMY` requests with a normal "not configured"
    /// rejection. One service per server, shared by clone with every
    /// shard (the platform's economy is not a per-shard thing).
    economy: Option<EconomyService>,
}

impl ShardedRoom {
    /// Build shard `index` of a `shard_count`-shard room over a square
    /// map of half-size `half`. All shards of a room share `half`.
    pub fn new(index: usize, shard_count: usize, spawn_half: f32) -> Self {
        let (rows, cols) = grid_shape(shard_count);
        let half = spawn_half.max(1.0);
        let cell_w = 2.0 * half / cols as f32;
        let cell_h = 2.0 * half / rows as f32;
        let row = index / cols;
        let col = index % cols;
        let mut neighbors = Vec::with_capacity(4);
        if col > 0 {
            neighbors.push(index - 1);
        }
        if col + 1 < cols {
            neighbors.push(index + 1);
        }
        if row > 0 {
            neighbors.push(index - cols);
        }
        if row + 1 < rows {
            neighbors.push(index + cols);
        }
        Self {
            runner: crate::common::movement_runner(),
            index,
            shard_count,
            cols,
            half,
            cell_w,
            cell_h,
            border: (cell_w.min(cell_h)) / 4.0,
            neighbors,
            player_entity: HashMap::new(),
            park: crate::common::ParkPolicy::default(),
            park_ledger: HashMap::new(),
            entity_player: HashMap::new(),
            wire_entity: HashMap::new(),
            serial_used: 0,
            own_wires: HashSet::new(),
            border_cache: Vec::new(),
            last: HashMap::new(),
            encoded: 0,
            input: HashMap::new(),
            economy: None,
        }
    }

    /// Set the disconnect-park grace (see
    /// [`crate::room::OpenRoom::with_disconnect_grace`]; RECONNECT §3).
    /// Every shard of a room should carry the same policy (the factory
    /// builds them uniformly).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.park.grace = grace;
        self
    }

    /// Attach the economy service handle (the RPC pattern's external-I/O
    /// half on the sharded path — Faz 3; see [`Self::economy`]). Builder-
    /// style, like [`crate::room::OpenRoom::with_economy`]; every shard of
    /// a room gets a clone of the ONE server-wide service.
    #[must_use]
    pub fn with_economy(mut self, economy: EconomyService) -> Self {
        self.economy = Some(economy);
        self
    }

    /// This shard's region rectangle `[x0, x1] × [y0, y1]`.
    fn rect(&self) -> (f32, f32, f32, f32) {
        let row = self.index / self.cols;
        let col = self.index % self.cols;
        let x0 = -self.half + col as f32 * self.cell_w;
        let y0 = -self.half + row as f32 * self.cell_h;
        (x0, x0 + self.cell_w, y0, y0 + self.cell_h)
    }

    /// Mint the next identity from this shard's disjoint range (both
    /// wire ids and stable player ids draw from the ONE counter — see
    /// the `serial_used` field docs).
    fn mint(&mut self) -> u64 {
        self.serial_used += 1;
        self.index as u64 * SHARD_SERIAL_RANGE + self.serial_used
    }

    /// Mint the next STABLE player identity (Faz 2): range-partitioned
    /// like the wire ids, so two shards never mint the same player.
    fn mint_player(&mut self) -> PlayerId {
        PlayerId(self.mint())
    }

    /// Whether the (truncated) position `(x, y)` lies in the border frame
    /// of this shard's region (within `border` of the region rectangle,
    /// including the thin overlap into it): the filter that decides which
    /// borrowed records from the neighbors this shard's snapshots include
    /// (module docs, "Visibility model").
    fn in_border_frame(&self, x: i32, y: i32) -> bool {
        let (x0, x1, y0, y1) = self.rect();
        let b = self.border;
        x as f32 >= x0 - b
            && x as f32 <= x1 + b
            && y as f32 >= y0 - b
            && y as f32 <= y1 + b
    }
}

// Faz 1 trait split: the shared contract — snapshot groups, the tick
// seam, membership, the reconnect surface — implements the `GameLogic`
// supertrait; the sharding seam stays on `ShardLogic` below.
impl GameLogic<World> for ShardedRoom {
    type GroupKey = ();
    type Strip = StripPos;

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }

    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    /// One group per shard (see module docs, "Group key").
    fn group_of(&self, _world: &World, _player: PlayerId) -> Self::GroupKey {
        Default::default()
    }

    fn snapshot(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        _group: &Self::GroupKey,
        borrowed: &[BorderRecord<StripPos>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        // The shard's own world (wire id, truncated position), collected
        // while the query holds the world borrow.
        let mut own: Vec<(u64, i32, i32)> = Vec::new();
        {
            let mut query = world.query::<(&WireId, &Position)>();
            for (wire, pos) in query.iter(world) {
                own.push((wire.get(), pos.x as i32, pos.y as i32));
            }
        }

        // The content is the own world plus the borrowed boundary records
        // (the core has already sorted them by wire and filtered out any
        // that are this shard's own — the own record wins over the
        // neighbor's one-tick-stale copy of an entity that just crossed
        // in). The frame filter keeps only the records actually near this
        // shard (module docs, "Visibility model"): a neighbor's export
        // covers the neighbor's WHOLE boundary, and the parts of it far
        // from this shard (the neighbor's other edges) are not visible
        // here. "No change" includes the borrowed content: a neighbor's
        // boundary entity moving is a content change for this shard.
        let mut content: HashMap<u64, (i32, i32)> = HashMap::with_capacity(own.len());
        for (w, x, y) in &own {
            content.insert(*w, (*x, *y));
        }
        for rec in borrowed {
            if self.in_border_frame(rec.state.x, rec.state.y) {
                content.entry(rec.wire).or_insert((rec.state.x, rec.state.y));
            }
        }

        if self.last == content {
            return false;
        }

        let mut snap = crate::game::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(content.len()),
            removed: Vec::new(),
            cell_exits: Vec::new(),
            delta: false,
        };
        // Deterministic payload order (sort by wire — the own records are
        // in query order and the borrowed are already sorted; a single
        // sort over the merged content keeps the snapshot stable so the
        // ledger and the wire bytes are reproducible tick-to-tick).
        let mut entries: Vec<(&u64, &(i32, i32))> = content.iter().collect();
        entries.sort_unstable_by_key(|(w, _)| **w);
        for (w, &(x, y)) in entries {
            snap.entities.push(crate::game::EntityRecord {
                entity: *w,
                x,
                y,
            });
        }
        snap
            .encode(out)
            .expect("protobuf encode into an in-memory buffer failed");

        self.encoded += content.len() as u64;
        self.last = content;
        true
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        // The spawn point derives from the TRANSPORT session id (as it
        // always has — the load generator's home distribution pairs with
        // it); the stable player identity comes from this shard's
        // range-partitioned counter.
        let (x, y) = spawn_pos(conn, self.half);
        let player = self.mint_player();
        let wire = self.mint();
        let entity = world
            .spawn((
                Position { x, y },
                Speed(DEFAULT_SPEED),
                WireId::new(wire),
            ))
            .id();
        self.player_entity.insert(player, entity);
        self.entity_player.insert(entity, player);
        self.wire_entity.insert(wire, entity);
        self.own_wires.insert(wire);
        self.input.insert(player, crate::common::InputState::default());
        Admission { player, entity: wire }
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        if let Some(entity) = self.player_entity.remove(&player)
            && world.get_entity(entity).is_ok()
        {
            if let Some(wire) = world.get::<WireId>(entity).copied() {
                self.wire_entity.remove(&wire.get());
                self.own_wires.remove(&wire.get());
            }
            self.entity_player.remove(&entity);
            self.input.remove(&player);
            world.despawn(entity);
        }
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        // The bot's synthesized frames (RECONNECT §9), resolved through
        // this shard's own wire table — the same shared helper the
        // single-world rooms use.
        let bots = self
            .park_ledger
            .values()
            .filter(|e| e.bot)
            .filter_map(|e| self.wire_entity.get(&e.wire).map(|&en| (e.player, en)));
        crate::common::synthesize_bot_moves(bots, world, ctx, actions);
        crate::common::ingest(&self.player_entity, world, actions, &mut self.input)
    }

    // -- the disconnect policy (see `crate::room::OpenRoom`, the shared
    //    hook bodies live in `crate::common`; this shard-side mirror keys
    //    its ledger by identity like the others but tracks the WIRE id,
    //    because that is what survives migrations) ----------------------

    fn on_disconnect(
        &mut self,
        world: &mut World,
        player: PlayerId,
        identity: &str,
    ) -> Detach {
        if self.park.grace.is_zero() || identity.is_empty() {
            return Detach::Despawn;
        }
        match self.player_entity.get(&player) {
            Some(&entity) => {
                let wire = world
                    .entity(entity)
                    .get::<WireId>()
                    .map(|w| w.get())
                    .unwrap_or_default();
                if wire != 0 {
                    self.park_ledger.insert(
                        identity.to_string(),
                        ShardParkRecord {
                            identity: identity.to_string(),
                            player,
                            wire,
                            bot: false,
                        },
                    );
                }
                Detach::Hold {
                    grace: Some(self.park.grace),
                    to: gsb_core::room::ExpireTo::AiHandover,
                }
            }
            None => Detach::Despawn,
        }
    }

    fn on_detach_expired(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        to: gsb_core::room::ExpireTo,
    ) {
        match to {
            gsb_core::room::ExpireTo::Despawn => {
                self.park_ledger.retain(|_, e| e.player != player);
            }
            gsb_core::room::ExpireTo::AiHandover => {
                for e in self.park_ledger.values_mut().filter(|e| e.player == player) {
                    e.bot = true;
                }
            }
        }
    }

    fn resume_lookup(&self, _world: &World, identity: &str) -> ResumeFound {
        match self.park_ledger.get(identity) {
            Some(e) => ResumeFound::Held(e.player),
            None => ResumeFound::Never,
        }
    }

    fn on_resume(
        &mut self,
        _world: &mut World,
        identity: &str,
        _conn: ConnectionId,
        player: PlayerId,
        _entity: EntityId,
    ) {
        // Faz 2 shrink: consume the ledger entry + seq/ack reset. Nothing
        // to re-key — every table is keyed by the STABLE player id.
        self.park_ledger.remove(identity);
        self.input.remove(&player);
    }

    /// The per-connection private frame: the pending input
    /// acknowledgment (Section A) and this tick's queued RPC answers
    /// (Faz 3 — same-tick local replies plus later-tick worker reports /
    /// timeout sweeps; the shard actor queues them per session). The
    /// shard's snapshots are full, self-contained (one group per shard),
    /// so there is nothing else per-connection to deliver.
    fn private(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        _group: &(),
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        crate::common::emit_private(&mut self.input, player, responses, out)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::common::run_systems(&mut self.runner, world, ctx);

        // Orphan stamping, range-aware (the demo game has no NPCs, but the
        // broadcast set stays structural — "has a Position" — like the
        // other rooms): entities with a `Position` but no `WireId` get
        // the next serial FROM THIS SHARD'S RANGE (a shared counter would
        // mint ids outside the range and break the disjointness invariant).
        let orphans: Vec<Entity> = world
            .query_filtered::<(Entity, &Position), bevy_ecs::prelude::Without<WireId>>()
            .iter(world)
            .map(|(e, _)| e)
            .collect();
        for entity in orphans {
            let wire = self.mint();
            world.entity_mut(entity).insert(WireId::new(wire));
            self.wire_entity.insert(wire, entity);
            self.own_wires.insert(wire);
        }

        // Rebuild the border cache (positions just changed in the movement
        // system; `collect_border` cannot query — it takes `&World`).
        let (x0, x1, y0, y1) = self.rect();
        let b = self.border;
        self.border_cache.clear();
        let mut query = world.query::<(&WireId, &Position)>();
        for (wire, pos) in query.iter(world) {
            let near = (pos.x - x0) < b
                || (x1 - pos.x) < b
                || (pos.y - y0) < b
                || (y1 - pos.y) < b;
            if near {
                self.border_cache.push(BorderRecord {
                    wire: wire.get(),
                    state: StripPos {
                        x: pos.x as i32,
                        y: pos.y as i32,
                    },
                });
            }
        }
    }

    /// The demo's two request kinds on the SHARDED path (Faz 3 — the same
    /// contract as [`crate::room::OpenRoom::handle_request`], resolved
    /// against THIS shard's world):
    ///
    /// - `ABILITY` (room-local): range check + a real world mutation (the
    ///   entity gets a `MoveTarget`), answered in the same tick's private
    ///   frame. The requester is looked up by its STABLE player id, so a
    ///   session that resumed onto this shard resolves identically.
    /// - `ECONOMY` (external I/O): delegated to the economy service via
    ///   an owning future; the shard actor registers it pending and the
    ///   answer rides a later tick's private path. A migration of the
    ///   requesting session mid-flight drops its pending state at
    ///   migrate-out (`gsb_core::shard` module docs) — the answer is
    ///   forfeited by design, exactly like a detach.
    fn handle_request(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        req: &gsb_core::rpc::RpcRequest,
    ) -> Option<RequestDecision> {
        match req.op {
            op::ABILITY => {
                let Ok(use_msg) = <crate::game::AbilityUse as Message>::decode(&req.payload[..])
                else {
                    return Some(RequestDecision::Reject(
                        "undecodable AbilityUse payload".into(),
                    ));
                };
                let Some(entity) = self.player_entity.get(&req.player).copied() else {
                    return Some(RequestDecision::Reject("no entity for this connection".into()));
                };
                let Ok(he) = world.get_entity(entity) else {
                    return Some(RequestDecision::Reject("entity already gone".into()));
                };
                let Some(pos) = he.get::<Position>().copied() else {
                    return Some(RequestDecision::Reject("entity has no position".into()));
                };
                let dx = use_msg.x as f32 - pos.x;
                let dy = use_msg.y as f32 - pos.y;
                const RANGE: f32 = 10.0;
                if dx * dx + dy * dy > RANGE * RANGE {
                    return Some(RequestDecision::Reject(format!(
                        "target out of range ({} > {RANGE})",
                        (dx * dx + dy * dy).sqrt()
                    )));
                }
                world
                    .entity_mut(entity)
                    .insert(MoveTarget { x: use_msg.x as f32, y: use_msg.y as f32 });
                let res = crate::game::AbilityResult {
                    ok: true,
                    reason: String::new(),
                };
                Some(RequestDecision::Reply(res.encode_to_vec().into()))
            }
            op::ECONOMY => {
                let Ok(buy) = <crate::game::BuyItem as Message>::decode(&req.payload[..]) else {
                    return Some(RequestDecision::Reject("undecodable BuyItem payload".into()));
                };
                let Some(economy) = self.economy.clone() else {
                    return Some(RequestDecision::Reject(
                        "economy service not configured".into(),
                    ));
                };
                // An OWNING future (a cheap sender clone inside): borrows
                // nothing from the shard (the `External` contract).
                let fut = async move {
                    match economy.buy(buy.kind).await {
                        Ok(price) => {
                            let res = crate::game::BuyResult {
                                ok: true,
                                reason: String::new(),
                                price,
                            };
                            Ok(res.encode_to_vec().into())
                        }
                        Err(reason) => Err(reason),
                    }
                };
                Some(RequestDecision::External(Box::pin(fut)))
            }
            // Not a request op this logic handles: the core answers with
            // a normal "no handler" rejection.
            _ => None,
        }
    }

    /// This shard's match result (the Faz 3 promotion; the per-shard
    /// sibling of [`crate::room::OpenRoom::match_result`]): the FINAL
    /// snapshot of this shard's own region at teardown. One logical room
    /// therefore yields one such payload PER SHARD through the shared
    /// sink (all under the logical room id — the platform adapter
    /// concatenates/filters); the shards' ranges are disjoint, so the
    /// concatenated entity set is collision-free by construction.
    fn match_result(&mut self, world: &mut World) -> Option<bytes::Bytes> {
        let mut entities: Vec<crate::game::EntityRecord> = Vec::new();
        {
            let mut query = world.query::<(&WireId, &Position)>();
            for (wire_id, pos) in query.iter(world) {
                entities.push(crate::game::EntityRecord {
                    entity: wire_id.get(),
                    x: pos.x as i32,
                    y: pos.y as i32,
                });
            }
        }
        entities.sort_by_key(|e| e.entity);
        let snap = crate::game::WorldSnapshot {
            // The shutdown snapshot has no live ticker: sequence 0 marks
            // "terminal" (live snapshots are strictly positive ticks).
            sequence: 0,
            entities,
            removed: Vec::new(),
            cell_exits: Vec::new(),
            delta: false,
        };
        let mut out = bytes::BytesMut::new();
        snap.encode(&mut out)
            .expect("protobuf encode into an in-memory buffer failed");
        Some(out.freeze())
    }
}

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
        let mut query = world
            .query::<(Entity, &WireId, &Position, &Speed, Option<&MoveTarget>)>();
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
        let entity = world
            .spawn((state.pos, Speed(state.speed), WireId::new(wire)))
            .id();
        if let Some(target) = state.target {
            world.entity_mut(entity).insert(target);
        }
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

/// The region (shard index) of a position for this shard's grid.
impl ShardedRoom {
    fn region_of(&self, pos: Position) -> usize {
        shard_at(pos.x, pos.y, self.half, self.shard_count)
    }
}

/// The `sharded × spatial` composite (ROADMAP Faz B — see the module
/// docs, "The spatial composite"): the grid topology of
/// [`ShardedRoom`] with each shard's broadcast phase re-grouped by
/// spatial cell and delta-encoded against last-sent content — AoiRoom's
/// engine ([`crate::common::CellBook`] / [`crate::common::CellPieces`])
/// driven per shard, plus THE borrowed-strip ledger that keeps a static
/// border silent.
///
/// Composition, not duplication: everything the grid protocol owns
/// (minting, migration state, park ledger, border cache, RPC plumbing,
/// economy) lives in the wrapped [`ShardedRoom`] and is delegated; this
/// type adds only the spatial broadcast surface (group key, packets,
/// view baselines) and the strip integration.
pub struct ShardedSpatialRoom {
    /// The grid-protocol half (delegated hooks; same module, so its
    /// private tables are readable where the seam requires it).
    inner: ShardedRoom,
    /// World units per cell edge (the config's `aoi_cell_size`; the same
    /// knob the single-world AOI room turns).
    cell_size: f32,
    /// The content bookkeeping shared with [`crate::aoi::AoiRoom`] —
    /// buckets over OWN entities AND borrowed records alike, change
    /// lists, member counts, born groups. Fed from two sources: the
    /// bevy dirty pass in `update` (own entities) and
    /// [`Self::integrate_borrowed`] (the strip diff).
    book: CellBook,
    /// THE ledger (module docs, "THE borrowed-strip × delta-ledger
    /// subtlety"): the previous tick's flattened borrowed view,
    /// `wire → (x, y)` truncated. The new slice is diffed against THIS,
    /// never against the buckets, so an unchanged strip dirties nothing.
    prev_borrowed: HashMap<u64, (i32, i32)>,
    /// Once-per-tick guard for the strip integration + deferred roll:
    /// the tick whose broadcast-phase preparation has already run.
    integrated_tick: u64,
    /// Per-player view baseline (`player → the cell whose FULL view was
    /// last delivered to it`): missing/other ⇒ the one-shot private
    /// full. Cleared on join/resume/migrate-in/migrate-out — a fresh
    /// session or a fresh shard MUST re-baseline (module docs, "Migration
    /// correctness").
    conn_view: HashMap<PlayerId, Cell>,
    /// The global tick of the current step (set in `update`).
    tick: u64,
    // ── Per-tick piece caches (cleared in `update`, computed lazily in
    //    the broadcast phase; order-independent across groups). ──
    pieces: CellPieces,
    /// The groups that emitted a FULL this tick (fresh group /
    /// keepalive): their members' private frames skip the one-shot.
    group_full_emitted: HashSet<Cell>,
}

impl ShardedSpatialRoom {
    /// Build shard `index` of a `shard_count`-shard room over a square
    /// map of half-size `half`, broadcasting with cells of `cell_size`
    /// world units (see [`ShardedRoom::new`] for the shared halves).
    pub fn new(index: usize, shard_count: usize, spawn_half: f32, cell_size: f32) -> Self {
        Self {
            inner: ShardedRoom::new(index, shard_count, spawn_half),
            cell_size: cell_size.max(0.5),
            book: CellBook::default(),
            prev_borrowed: HashMap::new(),
            integrated_tick: 0,
            conn_view: HashMap::new(),
            tick: 0,
            pieces: CellPieces::default(),
            group_full_emitted: HashSet::new(),
        }
    }

    /// Set the disconnect-park grace on the wrapped shard (builder-style,
    /// like [`ShardedRoom::with_disconnect_grace`]; every shard of a room
    /// should carry the same policy).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.inner = self.inner.with_disconnect_grace(grace);
        self
    }

    /// Attach the economy service handle (see [`ShardedRoom::with_economy`]).
    #[must_use]
    pub fn with_economy(mut self, economy: EconomyService) -> Self {
        self.inner = self.inner.with_economy(economy);
        self
    }

    /// THE strip integration (module docs, "THE borrowed-strip ×
    /// delta-ledger subtlety") plus the deferred occupancy/birth roll:
    /// idempotent per tick, invoked at the top of every broadcast-phase
    /// hook. Diff the NEW borrowed slice against [`Self::prev_borrowed`]
    /// — entered/exited/moved only; an identical wire position records
    /// NOTHING (the evaporation guard) — feed the diff into the shared
    /// bookkeeping as non-member content, then roll the flags against
    /// the final bucket state.
    fn integrate_borrowed(&mut self, borrowed: &[BorderRecord<StripPos>]) {
        if self.integrated_tick == self.tick {
            return;
        }
        let mut new_view: HashMap<u64, (i32, i32)> =
            HashMap::with_capacity(borrowed.len());
        for rec in borrowed {
            let pos = (rec.state.x, rec.state.y);
            new_view.insert(rec.wire, pos);
            match self.prev_borrowed.get(&rec.wire).copied() {
                None => {
                    // Entered the visible set (first contact, a healing
                    // Full after quarantine, or a crossing-in): an upsert
                    // in its containing cell — borrowed content joins the
                    // cell's group content for members of that cell.
                    let c = cell_of(rec.state.x, rec.state.y, self.cell_size);
                    self.book.record_appearance(rec.wire, rec.state.x, rec.state.y, c, false);
                }
                Some(prev) if prev != pos => {
                    // Moved: one upsert — or exit+upsert when the move
                    // crossed a cell boundary (the packet passes fix the
                    // wire order).
                    let old_c = cell_of(prev.0, prev.1, self.cell_size);
                    let new_c = cell_of(rec.state.x, rec.state.y, self.cell_size);
                    if old_c == new_c {
                        self.book.record_update(new_c, rec.wire, rec.state.x, rec.state.y);
                    } else {
                        self.book.record_cross(
                            old_c,
                            new_c,
                            rec.wire,
                            rec.state.x,
                            rec.state.y,
                            false,
                        );
                    }
                }
                Some(_) => {
                    // Unchanged since the previous tick: NOT a change —
                    // no dirtying, no upsert, no re-carrier (this arm is
                    // why the delta savings survive at the seams).
                }
            }
        }
        // Exited the visible set (left the neighbor's strip, the neighbor
        // migrated it onward, or its view went quarantined): exits in the
        // cells their previous records occupied.
        for (wire, &(px, py)) in &self.prev_borrowed {
            if !new_view.contains_key(wire) {
                let c = cell_of(px, py, self.cell_size);
                self.book.record_exit(c, *wire, false);
            }
        }
        self.prev_borrowed = new_view;
        // Every content source of the tick has landed (own dirty pass +
        // removals ran in `update`; the strip diff above) — NOW the
        // appeared/exited/birth classification is sound.
        self.book.roll();
        self.integrated_tick = self.tick;
    }

    /// Broadcast-phase precondition: integrate this tick's strip before
    /// any packet/full/baseline work reads the bookkeeping (idempotent —
    /// snapshot runs once per group, the integration must run once per
    /// tick).
    fn ensure_ready(&mut self, ctx: &TickCtx, borrowed: &[BorderRecord<StripPos>]) {
        debug_assert_eq!(ctx.tick, self.tick, "update must precede broadcast");
        self.integrate_borrowed(borrowed);
    }

    /// The roll-only variant for hooks that receive NO strip (`keepalive`,
    /// `private`): by the actor's phase order a snapshot always preceded
    /// them this tick (the integration already ran), so this normally
    /// no-ops on the guard; should an ordering anomaly ever skip the
    /// snapshot pass, roll with whatever the own-entity passes landed —
    /// NEVER fabricate a diff from an empty slice (that would read as
    /// "everything exited").
    fn ensure_rolled(&mut self) {
        if self.integrated_tick != self.tick {
            self.book.roll();
            self.integrated_tick = self.tick;
        }
    }
}

// The shared contract with a CELL group key: every hook either delegates
// to the wrapped shard (grid protocol) or drives the shared cell-delta
// engine (broadcast surface).
impl GameLogic<World> for ShardedSpatialRoom {
    type GroupKey = Cell;
    type Strip = StripPos;

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }

    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    /// The connection's group is the cell its entity's records land in —
    /// read from the O(1) `last_cell` table (written by the dirty pass),
    /// world-position fallback for the pre-first-update window (a join or
    /// migration-in processed in the CONTROL phase of the very tick being
    /// broadcast).
    fn group_of(&self, world: &World, player: PlayerId) -> Cell {
        let Some(&entity) = self.inner.player_entity.get(&player) else {
            return Cell(0, 0);
        };
        if let Some(&c) = self.book.last_cell.get(&entity) {
            return c;
        }
        let pos = world
            .entity(entity)
            .get::<Position>()
            .copied()
            .unwrap_or_default();
        cell_of(pos.x as i32, pos.y as i32, self.cell_size)
    }

    /// This cell's packet over the shard's own region content PLUS the
    /// integrated borrowed strip: a fresh group gets the 3×3 full; an
    /// established group gets the delta passes — all through the shared
    /// engine (the strip's enters/exits/moves sit in the same change
    /// lists the own movers wrote, so nothing here knows the difference).
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        cell: &Cell,
        borrowed: &[BorderRecord<StripPos>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ensure_ready(ctx, borrowed);
        assemble_group_packet(
            &mut self.pieces,
            &self.book,
            cell,
            &mut self.group_full_emitted,
            ctx.tick,
            out,
        )
    }

    /// Keep-alive on the cadence tick: a freshly encoded FULL of the
    /// group's view (a cached payload would be a delta — meaningless to
    /// re-send), whether the group emitted this tick or not. Same
    /// recovery contract as [`crate::aoi::AoiRoom::keepalive`].
    fn keepalive(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        group: &Cell,
        _last: Option<&bytes::Bytes>,
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ensure_rolled();
        self.group_full_emitted.insert(*group);
        let full = self.pieces.full_view(&self.book.buckets, self.tick, group);
        out.extend_from_slice(&full);
        true
    }

    fn encoded_records(&mut self) -> u64 {
        self.pieces.take_encoded()
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        let admission = self.inner.on_join(world, conn);
        // Member bookkeeping for the birth arithmetic (the wrapped shard
        // owns the tables; the spatial layer owns membership).
        if let Some(&entity) = self.inner.player_entity.get(&admission.player) {
            self.book.members.insert(entity);
        }
        admission
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        // Park the despawn removal BEFORE delegating (the delegate
        // despawns): despawns are not component writes, so the dirty pass
        // cannot see them. A join+leave inside one tick parks nothing
        // (never bucketed — the `last_cell` guard).
        if let Some(&entity) = self.inner.player_entity.get(&player) {
            self.book.members.remove(&entity);
            let wire = world.entity(entity).get::<WireId>().map(|w| w.get());
            let cell = self.book.last_cell.get(&entity).copied();
            if let (Some(wire), Some(cell)) = (wire, cell) {
                self.book.pending_removals.push((entity, wire, cell));
            }
        }
        self.inner.on_leave(world, player);
        self.conn_view.remove(&player);
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        self.inner.ingest(world, ctx, actions)
    }

    fn on_disconnect(
        &mut self,
        world: &mut World,
        player: PlayerId,
        identity: &str,
    ) -> Detach {
        self.inner.on_disconnect(world, player, identity)
    }

    fn on_detach_expired(
        &mut self,
        world: &mut World,
        player: PlayerId,
        to: gsb_core::room::ExpireTo,
    ) {
        self.inner.on_detach_expired(world, player, to)
    }

    fn resume_lookup(&self, world: &World, identity: &str) -> ResumeFound {
        self.inner.resume_lookup(world, identity)
    }

    fn on_resume(
        &mut self,
        world: &mut World,
        identity: &str,
        conn: ConnectionId,
        player: PlayerId,
        entity: EntityId,
    ) {
        self.inner.on_resume(world, identity, conn, player, entity);
        // The resumed SESSION has no view baseline: the next private frame
        // delivers a fresh one-shot full (same contract as a re-join).
        self.conn_view.remove(&player);
    }

    /// The per-connection private frame: the one-shot FULL view for a
    /// connection without a baseline for its CURRENT cell (join, resume,
    /// cell crossing, migration arrival) — skipped when the group's own
    /// emission this batch was already a full — plus the ordinary input
    /// ack / queued RPC answers.
    fn private(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        group: &Cell,
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ensure_rolled();
        let c = *group;
        if self.conn_view.get(&player).copied() != Some(c) {
            if self.group_full_emitted.contains(&c) {
                // The group's own full is ahead of this frame in the same
                // batch: it already baselined the connection.
                self.conn_view.insert(player, c);
            } else {
                // The one-shot private full: pre-encoded WorldSnapshot
                // bytes inside the Private message's snapshot oneof
                // (field 2, length-delimited); queued RPC answers ride
                // the SAME frame (field 3).
                let full = self.pieces.full_view(&self.book.buckets, self.tick, &c);
                out.put_u8(0x12); // Private field 2 (snapshot), LEN
                encode_varint(full.len() as u64, out);
                out.extend_from_slice(&full);
                crate::common::append_responses(responses, out);
                self.conn_view.insert(player, c);
                return true;
            }
        }
        crate::common::emit_private(&mut self.inner.input, player, responses, out)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        // Grid half: systems, range-aware orphan stamping, border-cache
        // rebuild (positions just changed).
        self.inner.update(world, ctx);
        // Spatial half: clear the per-tick state, run the own-entity
        // dirty pass, apply parked removals — but do NOT roll yet: the
        // borrowed strip arrives later than `update` (module docs, "why
        // the occupancy/birth roll cannot live in `update`"); the first
        // broadcast-phase call integrates it and rolls against the final
        // content.
        self.book.begin_tick();
        self.pieces.begin_tick();
        self.group_full_emitted.clear();
        self.tick = ctx.tick;
        self.book.dirty_pass(world, self.cell_size);
        self.book.apply_removals();
        // Close this tick's bevy change window (the core never calls
        // this — there is no system scheduler here; see
        // `docs/DESIGN.md` §7).
        world.clear_trackers();
    }

    fn handle_request(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        req: &gsb_core::rpc::RpcRequest,
    ) -> Option<RequestDecision> {
        self.inner.handle_request(world, ctx, req)
    }

    fn match_result(&mut self, world: &mut World) -> Option<bytes::Bytes> {
        self.inner.match_result(world)
    }
}

// The sharding seam: pure delegation — the composite changes WHAT a
// shard broadcasts, not how the grid moves entities across itself.
impl ShardLogic<World> for ShardedSpatialRoom {
    type State = <ShardedRoom as ShardLogic<World>>::State;

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use bevy_ecs::prelude::World;
    use gsb_core::id::{ConnectionId, RoomId};
    use gsb_core::room::TickCtx;

    use super::*;

    fn ctx(tick: u64) -> TickCtx {
        TickCtx {
            room: RoomId(1),
            tick,
            dt: Duration::from_secs_f64(1.0 / 30.0),
        }
    }

    /// Place a player at an exact position (join, then move the entity)
    /// for a deterministic region assignment. Returns the wire id.
    fn place(
        world: &mut World,
        room: &mut ShardedRoom,
        conn: ConnectionId,
        x: f32,
        y: f32,
    ) -> u64 {
        let admission = room.on_join(world, conn);
        let entity = *room.player_entity.get(&admission.player).expect("registered");
        world.entity_mut(entity).insert(Position { x, y });
        admission.entity
    }

    fn snap_ids(out: &bytes::BytesMut) -> BTreeSet<u64> {
        crate::game::WorldSnapshot::decode(out.as_ref())
            .expect("snapshot payload")
            .entities
            .iter()
            .map(|e| e.entity)
            .collect()
    }

    /// Region partition: every map point owns exactly one shard, the
    /// regions tile the map (no gaps at the edges — clamping), and the
    /// grid is balanced (rows*cols = N).
    #[test]
    fn region_partition_tiles_the_map() {
        for n in [1usize, 2, 3, 4, 6, 8, 12, 16] {
            let (rows, cols) = grid_shape(n);
            assert_eq!(rows * cols, n, "grid covers all shards (n={n})");
            let half = 50.0;
            // A grid of points across the whole map (and just outside it —
            // clamping keeps the "exactly one owner" invariant).
            for i in 0..=100 {
                for j in 0..=100 {
                    let x = -60.0 + 120.0 * i as f32 / 100.0;
                    let y = -60.0 + 120.0 * j as f32 / 100.0;
                    let s = shard_at(x, y, half, n);
                    assert!((0..n).contains(&s), "owner in range (n={n}): {s}");
                }
            }
            // Every shard owns at least one interior point.
            let mut owned = BTreeSet::new();
            for i in 0..=50 {
                for j in 0..=50 {
                    let x = -50.0 + 100.0 * i as f32 / 50.0;
                    let y = -50.0 + 100.0 * j as f32 / 50.0;
                    owned.insert(shard_at(x, y, half, n));
                }
            }
            assert_eq!(owned.len(), n, "every shard has area (n={n})");
        }
    }

    /// Wire identity: the shards' ranges are disjoint and ids are stable
    /// under migration (migrated-in keeps its id; the two shards' mints
    /// never collide).
    #[test]
    fn wire_ranges_are_disjoint_and_stable() {
        let mut world0 = World::new();
        let mut world1 = World::new();
        let mut s0 = ShardedRoom::new(0, 4, 50.0);
        let mut s1 = ShardedRoom::new(1, 4, 50.0);

        let w0 = place(&mut world0, &mut s0, ConnectionId(1), -10.0, -10.0);
        let w1 = place(&mut world1, &mut s1, ConnectionId(2), 10.0, -10.0);
        assert!(w0 < SHARD_SERIAL_RANGE, "shard 0 in range 0: {w0}");
        assert!(
            (SHARD_SERIAL_RANGE..2 * SHARD_SERIAL_RANGE).contains(&w1),
            "shard 1 in range 1: {w1}"
        );
        assert_ne!(w0, w1, "disjoint ranges ⇒ no collision");

        // Migrate w0 from shard 0 into shard 1: the id is preserved.
        let entity0 = *s0.player_entity.get(&PlayerId(1)).unwrap();
        let state = ShardedRoomState {
            pos: world0.entity(entity0).get::<Position>().copied().unwrap(),
            speed: world0
                .entity(entity0)
                .get::<Speed>()
                .map(|s| s.0)
                .unwrap_or(DEFAULT_SPEED),
            target: world0.entity(entity0).get::<MoveTarget>().copied(),
            park: None,
        };
        s1.on_migrate_in(&mut world1, w0, state, Some(PlayerId(1)));
        let entity1 = *s1.player_entity.get(&PlayerId(1)).unwrap();
        assert_eq!(
            world1.entity(entity1).get::<WireId>().unwrap().get(),
            w0,
            "the migrated entity keeps its wire id"
        );
    }

    /// Migration: an entity crossing into a neighbor's region is reported
    /// to EXACTLY that neighbor with its full state (position, speed,
    /// target); `on_migrate_out` despawns it on the sender side.
    #[test]
    fn migration_reports_crossing_with_full_state() {
        let mut world = World::new();
        let mut s0 = ShardedRoom::new(0, 4, 50.0); // x in [-50, 0)
        let w = place(&mut world, &mut s0, ConnectionId(1), -1.0, -10.0);
        let entity = *s0.player_entity.get(&PlayerId(1)).unwrap();
        world.entity_mut(entity).insert(MoveTarget { x: 1.0, y: -10.0 });

        // Still in shard 0: no migration to shard 1 (or anyone).
        assert!(
            s0.collect_migrations(&mut world, 1).is_empty(),
            "no crossing yet"
        );

        // Move across the seam (x: -1 → +1): now in shard 1's region.
        world.entity_mut(entity).insert(Position { x: 1.0, y: -10.0 });
        let to1 = s0.collect_migrations(&mut world, 1);
        assert_eq!(to1.len(), 1, "crossing reported once");
        let m = &to1[0];
        assert_eq!(m.wire, w, "wire id carried");
        assert_eq!(m.state.pos.x, 1.0, "position carried");
        assert_eq!(m.state.target, Some(MoveTarget { x: 1.0, y: -10.0 }));
        assert_eq!(m.player, Some(PlayerId(1)), "stable player carried");
        // Not reported to the other neighbors.
        assert!(s0.collect_migrations(&mut world, 2).is_empty());
        assert!(s0.collect_migrations(&mut world, 3).is_empty());

        // On the sender side, migrate-out despawns and cleans the tables.
        s0.on_migrate_out(&mut world, w);
        assert!(world.get_entity(entity).is_err(), "despawned on sender");
        assert!(!s0.player_entity.contains_key(&PlayerId(1)));
        assert!(!s0.wire_entity.contains_key(&w));
    }

    /// Boundary visibility: an entity within the border margin of the
    /// shared edge appears in the neighbor's snapshot (via the borrowed
    /// records), so a player at the seam sees across it; an entity deep in
    /// the shard (beyond the margin from the seam) does not. The export
    /// covers entities near ANY edge of the shard — it is the consumer's
    /// *frame filter* that decides what is actually visible (see the
    /// `frame_filter_discards_far_neighbor_edges` test for that side).
    #[test]
    fn border_visibility_across_the_seam() {
        let half = 50.0;
        // 1 row × 2 cols: shard 0 = x in [-50,0], y in [-50,50]; shard 1 =
        // x in [0,50], y in [-50,50]. Border = min(cell_w,cell_h)/4 = 12.5.
        let mut w0 = World::new();
        let mut w1 = World::new();
        let mut s0 = ShardedRoom::new(0, 2, half);
        let mut s1 = ShardedRoom::new(1, 2, half);

        let near = place(&mut w0, &mut s0, ConnectionId(1), -1.0, -10.0); // 1 from the seam
        let far = place(&mut w0, &mut s0, ConnectionId(2), -40.0, -10.0); // 40 from the seam
        // Mirror the actor order: `update` (rebuilds the border cache)
        // before the border export.
        s0.update(&mut w0, &ctx(1));

        // Shard 1's snapshot (its own world is empty here) includes the
        // borrowed records that pass its frame filter.
        let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
        let mut out = bytes::BytesMut::new();
        assert!(
            s1.snapshot(&mut w1, &ctx(1), &(), &borrowed, &mut out),
            "neighbor emits with borrowed content"
        );
        let seen = snap_ids(&out);
        assert!(
            seen.contains(&near),
            "a player at the seam sees across it: {seen:?}"
        );
        assert!(
            !seen.contains(&far),
            "an entity 40 from the seam (beyond the 12.5 margin) is \
             invisible: {seen:?}"
        );
    }

    /// Frame filter: with a 4×4 grid, shard 0's EAST neighbor (shard 1)
    /// exports its whole boundary; the parts of it near shard 1's OTHER
    /// edges (far from shard 0) must not leak into shard 0's snapshots.
    #[test]
    fn frame_filter_discards_far_neighbor_edges() {
        // 4×4: cell = 25, border = 6.25. Shard 0: x,y in [-50,-25).
        // Shard 1 (east): x in [-25,0), y in [-50,-25).
        let half = 50.0;
        let mut w0 = World::new();
        let mut w1 = World::new();
        let mut s0 = ShardedRoom::new(0, 16, half);
        let mut s1 = ShardedRoom::new(1, 16, half);

        // An entity in shard 1 near its WEST edge (the seam with shard 0).
        let seam = place(&mut w1, &mut s1, ConnectionId(1), -24.0, -37.5);
        // An entity in shard 1 near its EAST edge (far from shard 0).
        let east = place(&mut w1, &mut s1, ConnectionId(2), -1.0, -37.5);

        // Mirror the actor order: `update` (rebuilds the border cache)
        // before the border export.
        s1.update(&mut w1, &ctx(1));

        let border = s1.collect_border(&w1);
        let wires: BTreeSet<u64> = border.iter().map(|r| r.wire).collect();
        assert!(
            wires.contains(&seam) && wires.contains(&east),
            "both are within shard 1's border frame: {wires:?}"
        );

        // Shard 0's snapshot (its own world is empty here — only the
        // borrowed content matters): the seam entity is in its frame, the
        // east entity is 23+ units away (beyond the 6.25 margin) and
        // filtered out.
        let borrowed: Vec<BorderRecord<StripPos>> = border;
        let mut out = bytes::BytesMut::new();
        assert!(s0.snapshot(&mut w0, &ctx(1), &(), &borrowed, &mut out));
        let seen = snap_ids(&out);
        assert!(seen.contains(&seam), "seam entity visible: {seen:?}");
        assert!(
            !seen.contains(&east),
            "far neighbor edge filtered: {seen:?}"
        );
    }

    /// The snapshot is a self-contained, order-stable union of own and
    /// borrowed records; unchanged content (own + borrowed) stays silent.
    #[test]
    fn snapshot_union_and_no_change() {
        let mut w = World::new();
        let mut s = ShardedRoom::new(0, 4, 50.0);
        let a = place(&mut w, &mut s, ConnectionId(1), -10.0, -10.0);

        let borrowed = vec![BorderRecord {
            wire: 999,
            state: StripPos { x: -1, y: -10 },
        }];
        let mut out1 = bytes::BytesMut::new();
        assert!(s.snapshot(&mut w, &ctx(1), &(), &borrowed, &mut out1));
        let mut ids: Vec<u64> = snap_ids(&out1).into_iter().collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![a, 999], "own + borrowed union (sorted): {ids:?}");

        // Identical content (own + borrowed) ⇒ silent.
        let mut out2 = bytes::BytesMut::new();
        assert!(
            !s.snapshot(&mut w, &ctx(2), &(), &borrowed, &mut out2),
            "unchanged content ⇒ silent"
        );

        // A borrowed record moving is a content change ⇒ re-emit.
        let moved = vec![BorderRecord {
            wire: 999,
            state: StripPos { x: -1, y: -9 },
        }];
        let mut out3 = bytes::BytesMut::new();
        assert!(
            s.snapshot(&mut w, &ctx(3), &(), &moved, &mut out3),
            "borrowed movement ⇒ content change ⇒ emit"
        );
    }
    // ══ The Faz B spatial composite ([`ShardedSpatialRoom`]) ════════════
    //
    // The behavior locks of the `sharded × spatial` selection: cell
    // grouping per shard, seam continuity through the borrowed strip, the
    // evaporation guard (a static strip stays silent), and the
    // fresh-member rule for migration arrivals. `cell_size = 20`,
    // half = 50, 2 shards: s0 = x ∈ [-50, 0], s1 = x ∈ [0, 50]; border
    // margin 12.5; wire x=-1 → Cell(-1, ·), x=5 → Cell(0, ·), x=25 →
    // Cell(1, ·), x=45 → Cell(2, ·); y=-10 → row -1, y=15 → row 0.

    /// Join a player on a [`ShardedSpatialRoom`] and move its entity to an
    /// exact position. Returns the wire id.
    fn place_spatial(
        world: &mut World,
        room: &mut ShardedSpatialRoom,
        conn: ConnectionId,
        x: f32,
        y: f32,
    ) -> u64 {
        let admission = room.on_join(world, conn);
        let entity = *room.inner.player_entity.get(&admission.player).expect("registered");
        world.entity_mut(entity).insert(Position { x, y });
        admission.entity
    }

    /// Cells group members by position within ONE shard: a group sees its
    /// own cell plus the 3×3 ring — co-located and adjacent residents in,
    /// far cells out — and fresh groups open with a full packet.
    #[test]
    fn sharded_spatial_cells_group_members_by_position() {
        let mut world = World::new();
        let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
        let a = place_spatial(&mut world, &mut s1, ConnectionId(1), 5.0, -10.0); // Cell(0,-1)
        let b = place_spatial(&mut world, &mut s1, ConnectionId(2), 25.0, -10.0); // Cell(1,-1)
        let c = place_spatial(&mut world, &mut s1, ConnectionId(3), 45.0, -10.0); // Cell(2,-1)
        s1.update(&mut world, &ctx(1));

        let mut out = bytes::BytesMut::new();
        assert!(
            s1.snapshot(&mut world, &ctx(1), &Cell(0, -1), &[], &mut out),
            "A's group emits (fresh)"
        );
        let snap = crate::game::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
        assert!(!snap.delta, "a fresh group's first packet is a full");
        let seen: BTreeSet<u64> = snap.entities.iter().map(|e| e.entity).collect();
        assert!(seen.contains(&a) && seen.contains(&b), "own + adjacent visible: {seen:?}");
        assert!(!seen.contains(&c), "two cells away is outside the 3×3: {seen:?}");

        let mut out2 = bytes::BytesMut::new();
        assert!(s1.snapshot(&mut world, &ctx(1), &Cell(2, -1), &[], &mut out2));
        let seen2: BTreeSet<u64> =
            crate::game::WorldSnapshot::decode(out2.as_ref()).expect("snapshot").entities.iter().map(|e| e.entity).collect();
        assert!(seen2.contains(&c) && seen2.contains(&b), "C's group mirrors: {seen2:?}");
        assert!(!seen2.contains(&a), "far member not leaked across cells: {seen2:?}");
    }

    /// A client-view accumulator with FULL/DELTA application semantics
    /// (upserts, per-entity removals, whole-cell forgets) — what an
    /// observer's connection holds after each packet.
    #[derive(Default)]
    struct ClientView {
        ents: HashMap<u64, (i32, i32)>,
    }

    impl ClientView {
        fn apply(&mut self, snap: &crate::game::WorldSnapshot, cell_size: f32) {
            if !snap.delta {
                self.ents.clear();
            }
            for ce in &snap.cell_exits {
                let exited = Cell(ce.x, ce.y);
                self.ents.retain(|_, &mut (x, y)| cell_of(x, y, cell_size) != exited);
            }
            for w in &snap.removed {
                self.ents.remove(w);
            }
            for e in &snap.entities {
                self.ents.insert(e.entity, (e.x, e.y));
            }
        }
    }

    /// Seam continuity: an observer beside the border sees the neighbor's
    /// strip entities CONTINUOUSLY while they move on the other side —
    /// every tick's applied view carries the mover at its CURRENT
    /// truncated position (the accepted one-tick alignment blink is not
    /// observable here because the exporter rebuilds its cache before the
    /// exchange and the receiver integrates before its broadcast).
    #[test]
    fn borrowed_border_entities_render_without_gap_across_seam() {
        let mut w0 = World::new();
        let mut w1 = World::new();
        let mut s0 = ShardedRoom::new(0, 2, 50.0);
        let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
        let m = place(&mut w0, &mut s0, ConnectionId(1), -1.0, -10.0);
        let _o = place_spatial(&mut w1, &mut s1, ConnectionId(2), 5.0, -10.0);

        // The neighbor mover walks along the seam INSIDE its own region
        // (no migration), staying inside the observer shard's frame.
        let walk = [(-1.0f32, -10.0f32), (-3.0, -12.0), (-6.0, -14.0), (-9.0, -11.0)];
        let mut view = ClientView::default();
        for (t, pos) in walk.iter().enumerate() {
            let tick = t as u64 + 1;
            let entity = *s0.player_entity.get(&PlayerId(1)).unwrap();
            w0.entity_mut(entity).insert(Position { x: pos.0, y: pos.1 });

            // Mirror the actor order on both shards: update → export →
            // update → broadcast-with-borrowed.
            s0.update(&mut w0, &ctx(tick));
            let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
            s1.update(&mut w1, &ctx(tick));
            let mut out = bytes::BytesMut::new();
            assert!(
                s1.snapshot(&mut w1, &ctx(tick), &Cell(0, -1), &borrowed, &mut out),
                "tick {tick}: the observer's group emits"
            );
            let snap = crate::game::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
            view.apply(&snap, 20.0);
            assert_eq!(
                view.ents.get(&m).copied(),
                Some((pos.0 as i32, pos.1 as i32)),
                "tick {tick}: the seam entity renders at its current position                  (no gap, no stale ghost)"
            );
        }
    }

    /// THE EVAPORATION GUARD (module docs, "THE borrowed-strip ×
    /// delta-ledger subtlety"): the borrowed slice arrives full every
    /// tick, yet STATIC strip records must not dirty any group — no
    /// upserts shipped tick after tick — while a genuine strip move still
    /// ships exactly that one record.
    #[test]
    fn delta_bookkeeping_ignores_unchanged_borrowed_strip() {
        let mut w0 = World::new();
        let mut w1 = World::new();
        let mut s0 = ShardedRoom::new(0, 2, 50.0);
        let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
        let p = place(&mut w0, &mut s0, ConnectionId(1), -1.0, -10.0); // Cell(-1,-1)
        let q = place(&mut w0, &mut s0, ConnectionId(2), -3.0, 15.0); // Cell(-1,0)
        let _o = place_spatial(&mut w1, &mut s1, ConnectionId(3), 5.0, -10.0); // Cell(0,-1)

        // Tick 1: first contact — everything enters once.
        s0.update(&mut w0, &ctx(1));
        let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
        s1.update(&mut w1, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(s1.snapshot(&mut w1, &ctx(1), &Cell(0, -1), &borrowed, &mut out));
        let seen: BTreeSet<u64> = crate::game::WorldSnapshot::decode(out.as_ref())
            .expect("snapshot").entities.iter().map(|e| e.entity).collect();
        assert!(seen.contains(&p) && seen.contains(&q), "strip baselined once: {seen:?}");

        // Ticks 2–3: the SAME slice arrives (full replacement every tick
        // — exactly the shape the naive port chokes on): silence, zero
        // encoded records.
        for tick in [2u64, 3] {
            s0.update(&mut w0, &ctx(tick));
            let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
            s1.update(&mut w1, &ctx(tick));
            let mut out = bytes::BytesMut::new();
            assert!(
                !s1.snapshot(&mut w1, &ctx(tick), &Cell(0, -1), &borrowed, &mut out),
                "tick {tick}: unchanged strip ⇒ silent"
            );
            assert!(out.is_empty(), "tick {tick}: no bytes at all");
            assert_eq!(
                s1.encoded_records(),
                0,
                "tick {tick}: NO upserts shipped for unchanged borrowed records"
            );
        }

        // Tick 4: one strip record moves — exactly that record ships.
        let entity_p = *s0.player_entity.get(&PlayerId(1)).unwrap();
        w0.entity_mut(entity_p).insert(Position { x: -5.0, y: -10.0 }); // same cell
        s0.update(&mut w0, &ctx(4));
        let borrowed: Vec<BorderRecord<StripPos>> = s0.collect_border(&w0);
        s1.update(&mut w1, &ctx(4));
        let mut out = bytes::BytesMut::new();
        assert!(s1.snapshot(&mut w1, &ctx(4), &Cell(0, -1), &borrowed, &mut out));
        let snap = crate::game::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
        assert!(snap.delta);
        assert_eq!(snap.entities.len(), 1, "only the mover re-carried: {snap:?}");
        assert_eq!(snap.entities[0].entity, p);
        assert_eq!((snap.entities[0].x, snap.entities[0].y), (-5, -10));
        assert!(snap.entities.iter().all(|e| e.entity != q), "static q untouched");
        assert_eq!(s1.encoded_records(), 1);
    }

    /// Migration correctness (module docs, "Migration correctness"): a
    /// player migrating INTO an established mid-cell arrives as a FRESH
    /// group member — the very next private frame is the one-shot FULL of
    /// their new view (the established group's own packet stayed a delta
    /// and could not have baselined them).
    #[test]
    fn migrated_player_gets_private_full_on_arrival() {
        use crate::game::private::Payload;

        let mut w1 = World::new();
        let mut s1 = ShardedSpatialRoom::new(1, 2, 50.0, 20.0);
        let r = place_spatial(&mut w1, &mut s1, ConnectionId(1), 25.0, -10.0); // Cell(1,-1)
        s1.update(&mut w1, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(
            s1.snapshot(&mut w1, &ctx(1), &Cell(1, -1), &[], &mut out),
            "resident establishes the group"
        );

        // An arrival from the west shard into Cell(0,-1) — mid-cell, an
        // already-established neighborhood (its cell sits inside the
        // resident group's 3×3).
        let arrival_wire = 42_u64; // any id from another range (test-only)
        s1.on_migrate_in(
            &mut w1,
            arrival_wire,
            ShardedRoomState {
                pos: Position { x: 5.0, y: -10.0 },
                speed: DEFAULT_SPEED,
                target: None,
                park: None,
            },
            Some(PlayerId(9)),
        );
        s1.update(&mut w1, &ctx(2));

        // The ESTABLISHED resident group's packet is a delta carrying the
        // arrival's upsert — it does NOT baseline the arrival.
        out.clear();
        assert!(s1.snapshot(&mut w1, &ctx(2), &Cell(1, -1), &[], &mut out));
        let snap = crate::game::WorldSnapshot::decode(out.as_ref()).expect("snapshot");
        assert!(snap.delta, "established group stays in delta mode");
        assert!(snap.entities.iter().any(|e| e.entity == arrival_wire));

        // The arrival's private frame for ITS cell: the one-shot FULL.
        let mut pbuf = bytes::BytesMut::new();
        assert!(
            s1.private(&mut w1, PlayerId(9), &Cell(0, -1), &[], &mut pbuf),
            "the arrival receives the one-shot private full"
        );
        let frame = crate::game::Private::decode(pbuf.as_ref()).expect("private frame");
        let full = match frame.payload {
            Some(Payload::Snapshot(s)) => s,
            other => panic!("expected the snapshot oneof, got {other:?}"),
        };
        assert!(!full.delta, "the one-shot is a FULL");
        let seen: BTreeSet<u64> = full.entities.iter().map(|e| e.entity).collect();
        assert!(
            seen.contains(&arrival_wire) && seen.contains(&r),
            "the arrival sees itself AND the local resident immediately: {seen:?}"
        );

        // One-shot means one-shot.
        let mut pbuf2 = bytes::BytesMut::new();
        assert!(
            !s1.private(&mut w1, PlayerId(9), &Cell(0, -1), &[], &mut pbuf2),
            "the second private call ships nothing further"
        );
    }
}
