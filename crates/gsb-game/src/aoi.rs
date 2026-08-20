//! [`AoiRoom`]: an Area-of-Interest (AOI) [`RoomLogic`] for the demo game.
//!
//! ## What it changes (and what it deliberately does not touch)
//!
//! [`DemoRoom`](crate::room::DemoRoom) uses `GroupKey = ()`: one snapshot
//! group per room, so every connection receives the *entire* world —
//! per-connection bandwidth is O(entities in the room). `AoiRoom` makes the
//! group key a **spatial cell** (`GroupKey = [Cell]`): the snapshot group of
//! a connection is the cell its entity is in, and the room's *existing*
//! machinery then produces **one snapshot per cell and shares the same
//! `Bytes` with every connection in that cell**. Per-connection bandwidth
//! drops to O(visible entities).
//!
//! **No `gsb-core` change is required.** The group-snapshot architecture was
//! built for this: [`RoomLogic::group_of`](gsb_core::room::RoomLogic::group_of)
//! is re-evaluated every tick (so a player crossing a cell boundary
//! automatically changes group and starts receiving the new cell's snapshot),
//! the room encodes each group once and fans the frozen payload out by
//! reference (Arc refcount — "compute once per cell"), and "no change"
//! includes membership. `AoiRoom` only supplies a spatial `GroupKey`, a
//! `group_of` that maps a connection to its cell, and a `snapshot` that
//! encodes the cell's *visibility block*. The core already does the rest.
//!
//! ## Visibility set: the 3×3 neighborhood block
//!
//! A player's group is its own cell, but its *snapshot* is the **3×3 block**
//! centered on that cell (the cell plus its 8 ring-1 neighbors,
//! [`RADIUS`]). This is what resolves the boundary blind spot: a player at
//! the edge of cell C sees into the adjacent cells, so an enemy one cell
//! away (a meter over the boundary) is visible. The visibility boundary is
//! quantized to cells (you see a 3-cell-wide square, not 4 or 5); that is
//! inherent to cell-based AOI and is the price of the shared-bytes model.
//!
//! **Why a block keyed on the cell, and not a per-player radius?** A
//! per-player radius would make the snapshot a function of the *player*, not
//! the *cell* — every player in a cell would need a different payload, which
//! the group-snapshot model (one `Bytes` per group, shared by reference)
//! cannot express without either per-connection groups (`GroupKey =
//! ConnectionId`) or a core change. A block keyed on the cell keeps the
//! "compute once per cell, share the `Bytes`" property exactly: all players
//! in a cell see identical bytes.
//!
//! Alternatives considered and rejected:
//! - *Own cell only (no neighbors)* — the edge blind spot the spec calls out
//!   (a player on the boundary cannot see the neighbor cell). Rejected.
//! - *Per-player radius* — breaks the shared-bytes model (see above); would
//!   need a core change. Rejected for the "no core change" goal.
//! - *Larger block (5×5)* — wider visibility but ~2.8× the per-entity
//!   encoding cost and bigger payloads (more `max_snapshot_bytes` overflow).
//!   Rejected as the default; the block radius is a constant ([`RADIUS`])
//!   that can be widened if the game needs it.
//!
//! ## Cell size
//!
//! `cell_size` (world units per cell edge) is the one tunable. It is chosen
//! so that, at the expected *peak* entity density, a cell's 3×3 block stays
//! under `max_snapshot_bytes` (1400). A record is ~12 wire bytes, so a
//! visibility packet holds ~116 entities; with a 9-cell block that is ~13
//! entities per cell. Because the block is 9 cells, the entities-per-cell
//! budget shrinks as density rises — which is why `cell_size` must be
//! density-aware and is exposed as configuration rather than a magic
//! constant (a single constant cannot fit different arenas, densities, and
//! MTUs). See `docs/ROADMAP.md` for the measured break-even.
//!
//! ## The trade (measured, not assumed)
//!
//! AOI is not free. With E entities, C connections, G cells:
//! - *encoding*: AOI-off encodes the E-entity world **once** per tick.
//!   AOI-on encodes G blocks; each entity lands in the blocks of the cells
//!   whose 3×3 neighborhood contains it (up to 9, fewer for sparse/edge
//!   layouts), so total encoded entities is ~k·E (k≈block-overlap). Encoding
//!   CPU goes **up**.
//! - *bandwidth*: per connection, AOI-off ships E records; AOI-on ships the
//!   block's records (≪E). Fan-out count is still one batch per connection
//!   (unchanged), but each batch is smaller, so per-connection bytes and
//!   total outbound bytes go **down** by roughly the G/9 factor.
//!
//! The net win only materializes once G is large enough that the bandwidth
//! saving outweighs the extra encoding; for small G AOI is a net loss. The
//! load generator measures this (see `gsb-loadgen --aoi`).
//!
//! ## Invariants preserved (see `tests/aoi.rs`)
//!
//! - **Identity**: the wire id is minted once (`on_join` / orphan stamp in
//!   `update`) and never changes; a cell-changing entity keeps it.
//! - **Late join**: a joiner's entity enters the world in the control phase,
//!   so it is in the block that its cell emits on the first broadcast — the
//!   joiner sees its full visibility set.
//! - **Broadcast set**: the broadcast set is exactly "has a `Position`"
//!   (orphan stamping in `update`, structural like `DemoRoom`).
//! - **Self-contained**: no delta, no history; the "no change" ledger is
//!   keyed by cell (the `RoomLogic::snapshot` per-group bookkeeping
//!   contract), comparing exactly what the block carries.

use std::collections::HashMap;
use std::hash::Hash;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, EntityId};
use gsb_core::room::{Action, RoomLogic, TickCtx};
use gsb_ecs::SystemRunner;
use prost::Message;

use crate::components::{Position, WireId};
use crate::op;

/// A spatial cell of the world grid — the AOI group key. Cell indices are
/// the floor of (position / `cell_size`), so cells tile the plane and are
/// well-defined for negative coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell(pub i32, pub i32);

/// How many cells the visibility block extends in each direction from the
/// player's cell. `1` ⇒ a 3×3 block (the cell + its 8 ring-1 neighbors).
const RADIUS: i32 = 1;

/// The (dx, dy) offsets of the visibility block centered on a cell.
const BLOCK_OFFSETS: [(i32, i32); 9] = [
    (-RADIUS, -RADIUS), (0, -RADIUS), (RADIUS, -RADIUS),
    (-RADIUS, 0), (0, 0), (RADIUS, 0),
    (-RADIUS, RADIUS), (0, RADIUS), (RADIUS, RADIUS),
];

/// The cell containing `pos`, for a grid of `cell_size` (world units).
#[inline]
fn cell_of(pos: Position, cell_size: f32) -> Cell {
    Cell((pos.x / cell_size).floor() as i32, (pos.y / cell_size).floor() as i32)
}

/// The AOI room: spatial group key, per-cell visibility block, per-cell
/// "no change" ledger.
pub struct AoiRoom {
    runner: SystemRunner,
    /// Which entity belongs to which connection.
    conn_entity: HashMap<ConnectionId, Entity>,
    /// The room's single wire-identity counter (see module docs, "Identity").
    next_wire_id: u64,
    /// World units per cell edge (see module docs, "Cell size").
    cell_size: f32,
    /// Half-size of the square spawn map (see `gsb_game::room::spawn_pos`);
    /// configuration, not a strategy decision.
    spawn_half: f32,
    /// Per-cell "no change" ledger: `cell → (wire id → (x, y))`, the exact
    /// wire content of the cell's last emitted block. Keyed by group (cell)
    /// per the [`RoomLogic::snapshot`] contract: one call must not change
    /// another cell's answer in the same tick.
    last: HashMap<Cell, HashMap<u64, (i32, i32)>>,
    /// Per-tick bucket cache, rebuilt in [`Self::update`]: `cell → [(wire
    /// id, x, y)]`. Each entity is bucketed **once** per tick; a cell's
    /// visibility block is the union of the 9 buckets in its neighborhood,
    /// so the block is assembled by reference without re-querying the world.
    buckets: HashMap<Cell, Vec<(u64, i32, i32)>>,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `RoomLogic::encoded_records`).
    encoded: u64,
}

impl AoiRoom {
    /// Build an AOI room with the given `cell_size` (world units per cell
    /// edge) over the default 100×100 spawn arena. Clamped to a sane
    /// minimum so a degenerate `0` cannot produce a single infinite cell.
    #[must_use]
    pub fn new(cell_size: f32) -> Self {
        Self::with_spawn_half(cell_size, crate::room::DEFAULT_SPAWN_HALF)
    }

    /// Build an AOI room over a square spawn map of half-size `half` (see
    /// `gsb_game::room::DemoRoom::with_spawn_half`).
    #[must_use]
    pub fn with_spawn_half(cell_size: f32, half: f32) -> Self {
        Self {
            runner: crate::common::movement_runner(),
            conn_entity: HashMap::new(),
            next_wire_id: 0,
            cell_size: cell_size.max(0.5),
            spawn_half: half.max(1.0),
            last: HashMap::new(),
            buckets: HashMap::new(),
            encoded: 0,
        }
    }
}

impl RoomLogic<World> for AoiRoom {
    type GroupKey = Cell;

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    /// The connection's group is the cell its entity is in (re-evaluated
    /// every tick by the room — a crossing player changes cell and thus
    /// group, and starts receiving the new cell's block).
    fn group_of(&self, world: &World, conn: ConnectionId) -> Cell {
        let Some(&entity) = self.conn_entity.get(&conn) else {
            return Cell(0, 0);
        };
        let pos = world.entity(entity).get::<Position>().copied().unwrap_or_default();
        cell_of(pos, self.cell_size)
    }

    /// Encode the `cell`'s visibility block (its 3×3 neighborhood). The
    /// world is stable during the broadcast phase (the room ran `update`
    /// before any `snapshot`), so `self.buckets` is authoritative. Returns
    /// `false` when the block's wire content is unchanged since this
    /// cell's last emit (the per-cell ledger; membership changes are
    /// captured because an entity entering/leaving the block changes the
    /// content).
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        cell: &Cell,
        out: &mut bytes::BytesMut,
    ) -> bool {
        let mut content: HashMap<u64, (i32, i32)> = HashMap::new();
        for (dx, dy) in BLOCK_OFFSETS {
            if let Some(bucket) = self.buckets.get(&Cell(cell.0 + dx, cell.1 + dy)) {
                for &(wire_id, x, y) in bucket {
                    content.insert(wire_id, (x, y));
                }
            }
        }

        if let Some(prev) = self.last.get(cell)
            && *prev == content
        {
            return false;
        }

        let mut snap = crate::game::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(content.len()),
        };
        for (&wire_id, &(x, y)) in &content {
            snap.entities.push(crate::game::EntityRecord {
                entity: wire_id,
                x,
                y,
            });
        }
        // In-memory encode cannot fail; treat a failure as a bug.
        snap
            .encode(out)
            .expect("protobuf encode into an in-memory buffer failed");

        self.encoded += content.len() as u64;
        self.last.insert(*cell, content);
        true
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> EntityId {
        crate::common::on_join(
            &mut self.conn_entity,
            &mut self.next_wire_id,
            self.spawn_half,
            world,
            conn,
        )
    }

    fn on_leave(&mut self, world: &mut World, conn: ConnectionId) {
        crate::common::on_leave(&mut self.conn_entity, world, conn)
    }

    fn ingest(&mut self, world: &mut World, _ctx: &TickCtx, actions: &mut Vec<Action>) {
        crate::common::ingest(&self.conn_entity, world, actions)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::common::run_systems(&mut self.runner, world, ctx);

        // Orphan stamping (idempotent, mirrors `DemoRoom`): entities with a
        // `Position` but no `WireId` get the next serial, so the broadcast
        // set is exactly "has a `Position`" — structural, never silently
        // invisible. Done here (before the bucket build) so freshly-stamped
        // entities are in the buckets the broadcast phase reads.
        crate::common::stamp_orphans(&mut self.next_wire_id, world);

        // Bucket the world by cell, once per tick (each entity exactly
        // once); a cell's block is the union of its 3×3 neighborhood.
        self.buckets.clear();
        let mut query = world.query::<(&WireId, &Position)>();
        for (wire_id, pos) in query.iter(world) {
            let c = cell_of(*pos, self.cell_size);
            self.buckets
                .entry(c)
                .or_default()
                .push((wire_id.get(), pos.x as i32, pos.y as i32));
        }
    }
}

#[cfg(test)]
mod tests {
    //! Logic-level AOI tests (precise, direct `AoiRoom` calls; they need the
    //! room's private bookkeeping, so they live here rather than in
    //! `tests/aoi.rs`). `cell_size = 20` ⇒ 20×20 cells: (0,0)/(15,0) share
    //! `Cell(0,0)`; (45,0) is `Cell(2,0)` (outside a 3×3 block centered on
    //! `Cell(0,0)`); (100,0) is `Cell(5,0)` (far).

    use std::collections::BTreeSet;
    use std::time::Duration;

    use bevy_ecs::prelude::World;
    use gsb_core::id::{ConnectionId, RoomId};
    use gsb_core::room::TickCtx;
    use crate::components::{Position, Speed, DEFAULT_SPEED};

    use super::*;

    fn ctx(tick: u64) -> TickCtx {
        TickCtx {
            room: RoomId(1),
            tick,
            dt: Duration::from_secs_f64(1.0 / 30.0),
        }
    }

    /// Join a player (wire id assigned) and move its entity to an exact
    /// position for a deterministic cell placement. Returns the wire id.
    fn place(world: &mut World, room: &mut AoiRoom, conn: ConnectionId, x: f32, y: f32) -> u64 {
        let wire = room.on_join(world, conn);
        let entity = *room.conn_entity.get(&conn).expect("conn registered");
        world.entity_mut(entity).insert(Position { x, y });
        wire
    }

    fn snap_ids(out: &bytes::BytesMut) -> BTreeSet<u64> {
        crate::game::WorldSnapshot::decode(out.as_ref())
            .expect("snapshot payload")
            .entities
            .iter()
            .map(|e| e.entity)
            .collect()
    }

    /// Visibility set: a cell's block (its 3×3 neighborhood) contains the
    /// near entities but not the far ones.
    #[test]
    fn aoi_block_contains_near_not_far() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
        let b = place(&mut world, &mut room, ConnectionId(2), 15.0, 0.0); // Cell(0,0)
        let c = place(&mut world, &mut room, ConnectionId(3), 100.0, 0.0); // Cell(5,0)
        room.update(&mut world, &ctx(1));

        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));
        let near = snap_ids(&out);
        assert!(near.contains(&a) && near.contains(&b), "co-residents visible: {near:?}");
        assert!(!near.contains(&c), "far cell must not be visible: {near:?}");

        let mut out2 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(5, 0), &mut out2));
        let far = snap_ids(&out2);
        assert!(far.contains(&c), "C sees itself: {far:?}");
        assert!(!far.contains(&a) && !far.contains(&b), "far C does not see A/B: {far:?}");
    }

    /// Cell transition: an entity crossing a boundary changes group; the new
    /// cell's block now contains it (and its new neighbors), the old cell's
    /// no longer does — and its wire identity is unchanged throughout.
    #[test]
    fn aoi_cell_transition_block_and_identity() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
        let b = place(&mut world, &mut room, ConnectionId(2), 60.0, 0.0); // Cell(3,0)
        assert_ne!(a, b);

        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));
        assert!(snap_ids(&out).contains(&a));

        // A moves into B's cell (Cell(3,0)); identity must be preserved.
        let entity_a = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(entity_a).insert(Position { x: 60.0, y: 0.0 });
        room.update(&mut world, &ctx(2));

        let mut out_b = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(3, 0), &mut out_b), "new cell re-emits");
        let now_b = snap_ids(&out_b);
        assert!(now_b.contains(&a) && now_b.contains(&b), "co-resident in new cell: {now_b:?}");

        let mut out_a = bytes::BytesMut::new();
        room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out_a);
        let now_a = snap_ids(&out_a);
        assert!(!now_a.contains(&a), "A left its old cell's block: {now_a:?}");
    }

    /// Late join: a player entering an already-populated cell must, on that
    /// cell's next emit, see its full visibility block — the co-residents
    /// already there plus itself. The AOI analogue of the whole-world
    /// `late_join.rs` guarantee.
    #[test]
    fn aoi_late_join_sees_full_visibility_block() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0);
        let d = place(&mut world, &mut room, ConnectionId(2), 15.0, 0.0);
        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));

        // B joins the same cell late.
        let b = place(&mut world, &mut room, ConnectionId(3), 5.0, 0.0);
        room.update(&mut world, &ctx(2));

        let mut out2 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out2), "join re-emits");
        let seen = snap_ids(&out2);
        assert!(seen.contains(&a) && seen.contains(&d), "sees co-residents: {seen:?}");
        assert!(seen.contains(&b), "sees itself: {seen:?}");
    }

    /// Broadcast set: an entity spawned with a `Position` but no `WireId`
    /// (a bullet/NPC, not via `on_join`) is stamped in `update` and appears
    /// in its cell's block. The broadcast set is exactly "has a `Position`".
    #[test]
    fn aoi_broadcast_set_position_is_stamped() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0);
        let _orphan = world
            .spawn((Position { x: 3.0, y: 3.0 }, Speed(DEFAULT_SPEED)))
            .id();
        room.update(&mut world, &ctx(1));

        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));
        let ids = snap_ids(&out);
        assert!(ids.contains(&a), "resident present: {ids:?}");
        assert_eq!(ids.len(), 2, "orphan stamped and broadcast (2 entities): {ids:?}");
        assert!(!ids.contains(&0), "wire ids start at 1");
    }

    /// "No change" contract (self-contained snapshots, no delta/history): a
    /// block whose wire content is identical across two ticks is not
    /// re-encoded; a position change flips it back to "changed".
    #[test]
    fn aoi_no_change_when_block_static() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let _ = place(&mut world, &mut room, ConnectionId(1), 7.0, 9.0); // no MoveTarget
        let cell = Cell(0, 0);

        room.update(&mut world, &ctx(1));
        let mut out1 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &cell, &mut out1), "first emit");

        room.update(&mut world, &ctx(2));
        let mut out2 = bytes::BytesMut::new();
        assert!(!room.snapshot(&mut world, &ctx(2), &cell, &mut out2), "static block silent");
        assert!(out2.is_empty(), "no bytes written on silence");

        let entity = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(entity).insert(Position { x: 7.0, y: 19.0 });
        room.update(&mut world, &ctx(3));
        let mut out3 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(3), &cell, &mut out3), "movement re-emits");
    }
}
