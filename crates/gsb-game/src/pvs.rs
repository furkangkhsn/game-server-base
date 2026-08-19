//! [`SectorRoom`]: per-map-segment (PVS) [`RoomLogic`] for the demo game.
//!
//! ## What it changes (and what it deliberately does not touch)
//!
//! Visibility here is **not distance at all** — it is the map's own
//! geometry. The map is a set of hand-defined **convex sectors**
//! ([`SECTORS`]) plus the transitions between them, and a **precomputed
//! static visibility table** ([`VISIBLE_FROM`]): which sectors see which
//! sectors. An entity's group is the sector containing its position
//! (`GroupKey = Sector` — deliberately *not* `RoomId`, which already means
//! a gsb room; a sector is a region *inside* a room); a sector's snapshot
//! is the union of the entities of every sector the table says is visible
//! from it. The room's *existing* machinery then produces one snapshot per
//! sector and shares the `Bytes` with that sector's occupants — the same
//! "compute once per group, share by reference" property as `AoiRoom`.
//! **No `gsb-core` change.**
//!
//! **Why a static table lookup is the point.** A BSP compiler is *not*
//! written on purpose: for this demo the hand-written table *is* the
//! precomputation a real PVS pipeline (BSP/portal graphs) would produce.
//! What the seam proves is that "visibility" is a *static lookup over
//! hand-authored map data* — the runtime does `sector_of(position)` (a
//! point-in-convex test) and then `VISIBLE_FROM[sector]` (a table read),
//! never a distance comparison. That is what separates PVS from distance
//! AOI: two entities **3 units apart** do not see each other when their
//! sectors are unlinked in the table (a wall), and entities 60 units apart
//! DO see each other when their sectors are linked (a long sightline).
//! `tests/pvs.rs` pins both directions.
//!
//! ## The map (hand-written, convex, tiles the [-50, 50]² arena)
//!
//! ```text
//!        C | D            y
//!  -------+------  x=-10
//!   50    |      (north: C sees A and D; D sees B and C —
//!         |       an open north sightline)
//! -------A------B-----  y=20   (A↔C and B↔D are open passages)
//!  -50 -10|  x=0
//!         |            (A and B are GEOMETRICALLY adjacent — 3 units
//!   -50   |            apart at the closest — but the table says they
//!                 do NOT see each other: a wall. Distance-based AOI
//!                 cannot express this.)
//! ```
//!
//! - `A` (west), `B` (east): the two lobbies, split by a wall at `x = 0`
//!   (`y ∈ [-50, 20]`).
//! - `C` (northwest), `D` (northeast): the north band (`y ∈ [20, 50]`),
//!   split at `x = -10`.
//! - The sectors tile the arena plane, so every arena position is in
//!   exactly one sector (shared edges go to the first sector in the fixed
//!   test order — deterministic). A position outside every sector (a
//!   client sending a target beyond the map) lands in [`SECTOR_OUT`],
//!   which sees only itself: the broadcast set stays "has a `Position`"
//!   and a runaway entity can never leak into the map's visibility.
//!
//! ## Alternatives considered and rejected
//!
//! - *A real BSP/portal-graph compiler* — out of scope by the spec's own
//!   words ("for the demo a hand-written sector/transition table is
//!   enough"); it would add a build step and a second map format for no
//!   seam benefit: the runtime lookup shape (static table) is identical.
//! - *Axis-aligned rectangles only* — less expressive than the spec's
//!   "convex regions", and the general convex-polygon test is ~20 lines
//!   (`in_convex`), so restricting the geometry buys nothing.
//! - *A spatial index over the sectors* — unnecessary at this sector
//!   count (4 + OUT): the direct point-in-polygon test over the fixed
//!   list is clearer, and the lookup cost is O(sectors × edges) per
//!   entity per tick, which is negligible next to encoding/fan-out
//!   (measured in the load test).
//!
//! ## Invariants preserved (see `tests/pvs.rs` and the inline tests)
//!
//! - **Identity**: the wire id is minted once (`on_join` / orphan stamp in
//!   `update`) and never changes; a sector-changing entity keeps it.
//! - **Late join**: a joiner's entity enters the world in the control
//!   phase, so it is in the sector snapshot that its sector emits on the
//!   first broadcast — the joiner sees its sector's full visibility set.
//! - **Broadcast set**: exactly "has a `Position`" (orphan stamping in
//!   `update`, structural like `DemoRoom`).
//! - **Self-contained**: no delta, no history; the per-sector ledger
//!   compares exactly the wire content of that sector's last emitted
//!   snapshot (per-group bookkeeping contract), so an entity crossing a
//!   sector boundary *moves its record* between snapshots and the client
//!   reads "moved" from the full replacements alone.

use std::collections::HashMap;
use std::hash::Hash;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, EntityId};
use gsb_core::room::{Action, RoomLogic, TickCtx};
use gsb_ecs::SystemRunner;
use prost::Message;

use crate::components::{Position, WireId};
use crate::op;

/// A map sector — the PVS group key (a region *inside* a room; deliberately
/// not `RoomId`, which names a gsb room).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sector(pub u8);

/// The hand-written sectors of the demo map (module docs, "The map"):
/// convex polygons, counter-clockwise, tiling the [-50, 50]² arena.
const SECTOR_WEST: u8 = 0; /// `A`: x ∈ [-50, 0], y ∈ [-50, 20].
const SECTOR_EAST: u8 = 1; /// `B`: x ∈ [0, 50], y ∈ [-50, 20].
const SECTOR_NW: u8 = 2; /// `C`: x ∈ [-50, -10], y ∈ [20, 50].
const SECTOR_NE: u8 = 3; /// `D`: x ∈ [-10, 50], y ∈ [20, 50].
/// Positions outside every hand-written sector (a target beyond the map).
pub const SECTOR_OUT: u8 = 4;

/// The sector polygons (index = sector id, counter-clockwise winding).
const SECTORS: [[(f32, f32); 4]; 4] = [
    // A (west)
    [(-50.0, -50.0), (0.0, -50.0), (0.0, 20.0), (-50.0, 20.0)],
    // B (east)
    [(0.0, -50.0), (50.0, -50.0), (50.0, 20.0), (0.0, 20.0)],
    // C (northwest)
    [(-50.0, 20.0), (-10.0, 20.0), (-10.0, 50.0), (-50.0, 50.0)],
    // D (northeast)
    [(-10.0, 20.0), (50.0, 20.0), (50.0, 50.0), (-10.0, 50.0)],
];

/// The precomputed **static** visibility table: for each sector, a bitmask
/// of the sectors visible *from* it (itself included). This is the PVS —
/// the hand-authored map geometry (transitions/sightlines) frozen into a
/// table the runtime only reads.
///
/// - `A` sees `A` and `C` (open passage north of the west lobby).
/// - `B` sees `B` and `D` (open passage north of the east lobby).
/// - `C` sees `A`, `C` and `D` (plus the open north sightline to `D`).
/// - `D` sees `B`, `C` and `D`.
/// - `A` and `B` see NEITHER each other: they are geometrically adjacent
///   (3 units apart at the closest) but separated by a wall — the table
///   is the law, not the distance.
/// - `OUT` sees only itself (a runaway entity leaks into nothing).
const VISIBLE_FROM: [u16; 5] = [
    1 << SECTOR_WEST | 1 << SECTOR_NW,
    1 << SECTOR_EAST | 1 << SECTOR_NE,
    1 << SECTOR_WEST | 1 << SECTOR_NW | 1 << SECTOR_NE,
    1 << SECTOR_EAST | 1 << SECTOR_NW | 1 << SECTOR_NE,
    1 << SECTOR_OUT,
];

/// True when `(x, y)` is inside (or on the edge of) a counter-clockwise
/// convex polygon. A point is inside a convex polygon iff all edge
/// cross products have the same sign (edge points count as inside, so
/// shared sector boundaries are owned by the first sector in test order —
/// deterministic).
fn in_convex(poly: &[(f32, f32); 4], x: f32, y: f32) -> bool {
    let mut sign = 0.0f32;
    let n = poly.len();
    for i in 0..n {
        let (ax, ay) = poly[i];
        let (bx, by) = poly[(i + 1) % n];
        let cross = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
        if cross.abs() < 1e-9 {
            continue; // on the edge
        }
        let s = cross.signum();
        if sign == 0.0 {
            sign = s;
        } else if s != sign {
            return false;
        }
    }
    true
}

/// The sector containing `pos`: the first (fixed order) sector whose
/// polygon contains it, or [`SECTOR_OUT`] when outside all of them.
#[inline]
fn sector_of(pos: Position) -> Sector {
    for (i, poly) in SECTORS.iter().enumerate() {
        if in_convex(poly, pos.x, pos.y) {
            return Sector(i as u8);
        }
    }
    Sector(SECTOR_OUT)
}

/// The PVS room: sector group key, static-table visibility, per-sector
/// "no change" ledger.
pub struct SectorRoom {
    runner: SystemRunner,
    /// Which entity belongs to which connection.
    conn_entity: HashMap<ConnectionId, Entity>,
    /// The room's single wire-identity counter (mirrors the other rooms).
    next_wire_id: u64,
    /// Per-sector "no change" ledger: `sector → (wire id → (x, y))`, the
    /// exact wire content of that sector's last emitted snapshot. Keyed by
    /// group (sector) per the [`RoomLogic::snapshot`] contract.
    last: HashMap<Sector, HashMap<u64, (i32, i32)>>,
    /// Per-tick bucket cache, rebuilt in [`Self::update`]: `sector →
    /// [(wire id, x, y)]`. Each entity is bucketed **once** per tick; a
    /// sector's snapshot is the union of the buckets of the sectors
    /// [`VISIBLE_FROM`] says are visible from it, assembled by reference
    /// without re-querying the world.
    buckets: HashMap<Sector, Vec<(u64, i32, i32)>>,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `RoomLogic::encoded_records`).
    encoded: u64,
}

impl Default for SectorRoom {
    fn default() -> Self {
        Self::new()
    }
}

impl SectorRoom {
    /// Build a PVS room over the demo map (module docs, "The map").
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: crate::common::movement_runner(),
            conn_entity: HashMap::new(),
            next_wire_id: 0,
            last: HashMap::new(),
            buckets: HashMap::new(),
            encoded: 0,
        }
    }
}

impl RoomLogic<World> for SectorRoom {
    type GroupKey = Sector;

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    /// The connection's group is the sector its entity is in (re-evaluated
    /// every tick by the room — a crossing player changes sector and thus
    /// group, and starts receiving the new sector's snapshot).
    fn group_of(&self, world: &World, conn: ConnectionId) -> Sector {
        let Some(&entity) = self.conn_entity.get(&conn) else {
            return Sector(SECTOR_OUT);
        };
        let pos = world.entity(entity).get::<Position>().copied().unwrap_or_default();
        sector_of(pos)
    }

    /// Encode `sector`'s snapshot: the union of the buckets of every sector
    /// the static table says is visible from it (module docs). Returns
    /// `false` when that content is unchanged since this sector's last
    /// emit (per-sector ledger; membership and boundary crossings change
    /// the content).
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        sector: &Sector,
        out: &mut bytes::BytesMut,
    ) -> bool {
        let mut content: HashMap<u64, (i32, i32)> = HashMap::new();
        let mask = VISIBLE_FROM[sector.0 as usize];
        for s in 0..VISIBLE_FROM.len() as u8 {
            if mask & (1 << s) != 0
                && let Some(bucket) = self.buckets.get(&Sector(s))
            {
                for &(wire_id, x, y) in bucket {
                    content.insert(wire_id, (x, y));
                }
            }
        }

        if let Some(prev) = self.last.get(sector)
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
        self.last.insert(*sector, content);
        true
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> EntityId {
        crate::common::on_join(&mut self.conn_entity, &mut self.next_wire_id, world, conn)
    }

    fn on_leave(&mut self, world: &mut World, conn: ConnectionId) {
        crate::common::on_leave(&mut self.conn_entity, world, conn)
    }

    fn ingest(&mut self, world: &mut World, _ctx: &TickCtx, actions: &mut Vec<Action>) {
        crate::common::ingest(&self.conn_entity, world, actions)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::common::run_systems(&mut self.runner, world, ctx);

        // Orphan stamping (idempotent, mirrors the other rooms): entities
        // with a `Position` but no `WireId` get the next serial, so the
        // broadcast set is exactly "has a `Position`" — structural, never
        // silently invisible. Done here (before the bucket build) so
        // freshly-stamped entities are in the buckets the broadcast phase
        // reads.
        crate::common::stamp_orphans(&mut self.next_wire_id, world);

        // Bucket the world by sector, once per tick (each entity exactly
        // once); a sector's snapshot is the union of the buckets its
        // visibility table entry names.
        self.buckets.clear();
        let mut query = world.query::<(&WireId, &Position)>();
        for (wire_id, pos) in query.iter(world) {
            let s = sector_of(*pos);
            self.buckets
                .entry(s)
                .or_default()
                .push((wire_id.get(), pos.x as i32, pos.y as i32));
        }
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }
}

#[cfg(test)]
mod tests {
    //! Logic-level PVS tests (precise, direct `SectorRoom` calls; they
    //! need the room's private bookkeeping, so they live here rather than
    //! in `tests/pvs.rs`). Geometry: see the module docs ("The map").

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
    /// position for a deterministic sector placement. Returns the wire id.
    fn place(world: &mut World, room: &mut SectorRoom, conn: ConnectionId, x: f32, y: f32) -> u64 {
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

    /// The test that separates PVS from distance-based AOI: two entities
    /// **3 units apart** (sector A at x=-1, sector B at x=+2, the wall at
    /// x=0 between them) do NOT see each other, because A and B are
    /// unlinked in the visibility table. A distance-AOI with any radius
    /// >= 3 would let them see each other and cannot pass this test.
    #[test]
    fn unlinked_sectors_invisible_at_3_units() {
        let mut world = World::new();
        let mut room = SectorRoom::new();

        let p1 = place(&mut world, &mut room, ConnectionId(1), -1.0, 0.0); // A
        let p2 = place(&mut world, &mut room, ConnectionId(2), 2.0, 0.0);  // B
        room.update(&mut world, &ctx(1));

        // Pin the scenario: the two are exactly 3 units apart.
        assert_eq!(
            ((-1.0f32 - 2.0).abs(), (0.0f32 - 0.0).abs()),
            (3.0f32, 0.0f32),
            "the scenario is 3 units apart"
        );

        let mut out_a = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_WEST), &mut out_a));
        let a = snap_ids(&out_a);
        assert!(a.contains(&p1), "P1 in A's snapshot: {a:?}");
        assert!(!a.contains(&p2), "P2 (3 units away, unlinked sector) NOT visible: {a:?}");

        let mut out_b = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_EAST), &mut out_b));
        let b = snap_ids(&out_b);
        assert!(b.contains(&p2), "P2 in B's snapshot: {b:?}");
        assert!(!b.contains(&p1), "P1 (3 units away, unlinked sector) NOT visible: {b:?}");
    }

    /// Linked sectors are visible — regardless of distance: A↔C (15 units
    /// apart, open passage) and C↔D (7 units apart, north sightline).
    #[test]
    fn linked_sectors_visible() {
        let mut world = World::new();
        let mut room = SectorRoom::new();

        // The north band is split at x = -10: C covers x ∈ [-50, -10],
        // D covers x ∈ [-10, 50] — so "a point in C" needs x <= -10.
        let p1 = place(&mut world, &mut room, ConnectionId(1), -1.0, 10.0);   // A
        let p2 = place(&mut world, &mut room, ConnectionId(2), -20.0, 25.0);  // C (~24 from p1)
        let q1 = place(&mut world, &mut room, ConnectionId(3), -20.0, 30.0);  // C
        let q2 = place(&mut world, &mut room, ConnectionId(4), 2.0, 30.0);    // D (~22 from q1)
        room.update(&mut world, &ctx(1));

        let mut out_a = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_WEST), &mut out_a));
        let a = snap_ids(&out_a);
        // A sees A and C (both linked): p1, p2, q1 — but NOT q2 (in D,
        // unlinked with A).
        assert!(a.contains(&p1) && a.contains(&p2) && a.contains(&q1), "A sees A and C (linked): {a:?}");
        assert!(!a.contains(&q2), "A does not see D (unlinked): {a:?}");

        let mut out_c = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_NW), &mut out_c));
        let c = snap_ids(&out_c);
        assert!(c.contains(&p1) && c.contains(&p2) && c.contains(&q1) && c.contains(&q2),
            "C sees A and D (both linked): {c:?}");
    }

    /// Same sector is always visible — even at the sector's far corners
    /// (~67 units apart): the group is the sector, and a sector always
    /// sees itself. This is not a distance test (a small-radius AOI would
    /// hide these two).
    #[test]
    fn same_sector_always_visible_at_far_corners() {
        let mut world = World::new();
        let mut room = SectorRoom::new();

        let p1 = place(&mut world, &mut room, ConnectionId(1), -49.0, -49.0); // A, SW corner
        let p2 = place(&mut world, &mut room, ConnectionId(2), -1.0, 19.0);   // A, NE corner
        room.update(&mut world, &ctx(1));

        let mut out_a = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_WEST), &mut out_a));
        let a = snap_ids(&out_a);
        assert!(a.contains(&p1) && a.contains(&p2), "co-sector residents visible: {a:?}");
    }

    /// Sector transition: an entity crossing a boundary changes group; its
    /// record moves to the new sector's snapshot at the new position with
    /// the same wire identity, stays visible wherever the new sector is
    /// linked (A sees C), and remains hidden where the wall still stands
    /// (B does not see C) — the identity invariant across the PVS
    /// boundary crossing.
    #[test]
    fn sector_transition_snapshot_and_identity() {
        let mut world = World::new();
        let mut room = SectorRoom::new();

        let p1 = place(&mut world, &mut room, ConnectionId(1), -1.0, 0.0);  // A
        let p2 = place(&mut world, &mut room, ConnectionId(2), 2.0, 0.0);   // B
        room.update(&mut world, &ctx(1));

        let mut out_a = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_WEST), &mut out_a));
        assert!(snap_ids(&out_a).contains(&p1));

        // P1 moves into sector C (x <= -10, linked with A, NOT with B).
        let entity_p1 = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(entity_p1).insert(Position { x: -20.0, y: 25.0 });
        room.update(&mut world, &ctx(2));

        let mut out_c = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Sector(SECTOR_NW), &mut out_c),
            "new sector re-emits");
        let snap_c = crate::game::WorldSnapshot::decode(out_c.as_ref()).expect("snapshot");
        let now_c: BTreeSet<u64> = snap_c.entities.iter().map(|e| e.entity).collect();
        assert!(now_c.contains(&p1), "P1 in C's snapshot: {now_c:?}");
        let rec_c = snap_c.entities.iter().find(|e| e.entity == p1).expect("P1's record");
        assert_eq!((rec_c.x, rec_c.y), (-20, 25), "P1 at its new position in C's snapshot");

        // A sees C (linked): P1 stays in A's snapshot — its record MOVED
        // to the new position under the same wire id (C is linked with A,
        // so the crossing did not end A's visibility of P1).
        let mut out_a2 = bytes::BytesMut::new();
        room.snapshot(&mut world, &ctx(2), &Sector(SECTOR_WEST), &mut out_a2);
        let snap_a = crate::game::WorldSnapshot::decode(out_a2.as_ref()).expect("snapshot");
        let rec_a = snap_a
            .entities
            .iter()
            .find(|e| e.entity == p1)
            .expect("P1 still visible from A (A and C are linked)");
        assert_eq!((rec_a.x, rec_a.y), (-20, 25), "P1's record moved, id unchanged: {rec_a:?}");

        let mut out_b2 = bytes::BytesMut::new();
        room.snapshot(&mut world, &ctx(2), &Sector(SECTOR_EAST), &mut out_b2);
        let now_b = snap_ids(&out_b2);
        assert!(
            !now_b.contains(&p1),
            "P1 (now in C) is still invisible to B — the wall did not move: {now_b:?}"
        );
        assert!(
            now_b.contains(&p2),
            "B still sees itself: {now_b:?}"
        );
        // Identity: P1's wire id is the one assigned at join, unchanged
        // across the sector crossing.
        assert_eq!(p1, 1, "first entity gets wire id 1");
        assert!(now_c.contains(&1), "P1 keeps wire id 1 in C's snapshot");
    }

    /// "No change" contract (self-contained snapshots, no delta/history):
    /// a sector snapshot whose wire content is identical across two ticks
    /// is not re-encoded; a position change flips it back to "changed".
    #[test]
    fn sector_no_change_when_static() {
        let mut world = World::new();
        let mut room = SectorRoom::new();

        let _ = place(&mut world, &mut room, ConnectionId(1), 7.0, 9.0); // B, no MoveTarget
        let sector = Sector(SECTOR_EAST);

        room.update(&mut world, &ctx(1));
        let mut out1 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &sector, &mut out1), "first emit");

        room.update(&mut world, &ctx(2));
        let mut out2 = bytes::BytesMut::new();
        assert!(!room.snapshot(&mut world, &ctx(2), &sector, &mut out2), "static snapshot silent");
        assert!(out2.is_empty(), "no bytes written on silence");

        let entity = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(entity).insert(Position { x: 7.0, y: 19.0 });
        room.update(&mut world, &ctx(3));
        let mut out3 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(3), &sector, &mut out3), "movement re-emits");
    }

    /// Broadcast set + OUT: an entity with a `Position` but no `WireId`
    /// (spawned outside `on_join`) is stamped in `update` and appears in
    /// its sector's snapshot; an entity OUTSIDE the map (a runaway target)
    /// lands in `SECTOR_OUT`, sees only itself, and leaks into no map
    /// sector's snapshot — the broadcast set stays exactly "has a
    /// `Position`".
    #[test]
    fn orphan_stamped_and_outside_map_is_contained() {
        let mut world = World::new();
        let mut room = SectorRoom::new();

        let a = place(&mut world, &mut room, ConnectionId(1), 7.0, 9.0); // B
        let _orphan_in = world
            .spawn((Position { x: 8.0, y: 10.0 }, Speed(DEFAULT_SPEED)))
            .id(); // B
        let runaway = world
            .spawn((Position { x: 300.0, y: 300.0 }, Speed(DEFAULT_SPEED)))
            .id(); // outside the map
        room.update(&mut world, &ctx(1));

        let mut out_b = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_EAST), &mut out_b));
        let b = snap_ids(&out_b);
        assert_eq!(b.len(), 2, "B: resident + stamped orphan (the runaway is not here): {b:?}");
        assert!(b.contains(&a));

        let mut out_out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Sector(SECTOR_OUT), &mut out_out));
        let out_ids = snap_ids(&out_out);
        let runaway_id = room
            .last
            .get(&Sector(SECTOR_OUT))
            .and_then(|m| m.iter().next().map(|(k, _)| *k))
            .expect("OUT has exactly one occupant (the runaway)");
        assert!(out_ids.contains(&runaway_id), "OUT sees its occupant: {out_ids:?}");
        // The runaway must not appear in ANY map sector's snapshot.
        for s in 0..4u8 {
            let mut o = bytes::BytesMut::new();
            room.snapshot(&mut world, &ctx(1), &Sector(s), &mut o);
            let ids = snap_ids(&o);
            assert!(!ids.contains(&runaway_id), "sector {s} must not leak the runaway: {ids:?}");
        }
        let _ = runaway;
    }
}
