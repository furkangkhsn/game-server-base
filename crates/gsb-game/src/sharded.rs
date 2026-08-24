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

use std::collections::{HashMap, HashSet};

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, TickCtx};
use gsb_core::rpc::RequestDecision;
use gsb_core::shard::{BorrowedRecord, Migrating, ShardLogic, SHARD_SERIAL_RANGE};
use gsb_ecs::SystemRunner;
use prost::Message;

use crate::components::{DEFAULT_SPEED, MoveTarget, Position, Speed, WireId};
use crate::economy::EconomyService;
use crate::op;
use crate::room::spawn_pos;

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
    border_cache: Vec<BorrowedRecord>,
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
    /// [`crate::room::DemoRoom::with_disconnect_grace`]; RECONNECT §3).
    /// Every shard of a room should carry the same policy (the factory
    /// builds them uniformly).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.park.grace = grace;
        self
    }

    /// Attach the economy service handle (the RPC pattern's external-I/O
    /// half on the sharded path — Faz 3; see [`Self::economy`]). Builder-
    /// style, like [`crate::room::DemoRoom::with_economy`]; every shard of
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
        borrowed: &[BorrowedRecord],
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
            if self.in_border_frame(rec.x, rec.y) {
                content.entry(rec.wire).or_insert((rec.x, rec.y));
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

    // -- the disconnect policy (see `crate::room::DemoRoom`, the shared
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
                self.border_cache.push(BorrowedRecord {
                    wire: wire.get(),
                    x: pos.x as i32,
                    y: pos.y as i32,
                });
            }
        }
    }

    /// The demo's two request kinds on the SHARDED path (Faz 3 — the same
    /// contract as [`crate::room::DemoRoom::handle_request`], resolved
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
    /// sibling of [`crate::room::DemoRoom::match_result`]): the FINAL
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

    fn collect_border(&self, _world: &World) -> Vec<BorrowedRecord> {
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
        let borrowed: Vec<BorrowedRecord> = s0.collect_border(&w0);
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
        let borrowed: Vec<BorrowedRecord> = border;
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

        let borrowed = vec![BorrowedRecord {
            wire: 999,
            x: -1,
            y: -10,
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
        let moved = vec![BorrowedRecord {
            wire: 999,
            x: -1,
            y: -9,
        }];
        let mut out3 = bytes::BytesMut::new();
        assert!(
            s.snapshot(&mut w, &ctx(3), &(), &moved, &mut out3),
            "borrowed movement ⇒ content change ⇒ emit"
        );
    }
}
