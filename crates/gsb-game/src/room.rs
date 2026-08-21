//! [`DemoRoom`]: the demo game's [`RoomLogic`] implementation.
//!
//! A room owns one bevy [`World`] (exclusively — the room actor is the only
//! borrower) plus a small amount of bookkeeping:
//!
//! - `conn_entity`: which entity belongs to which connection;
//! - `next_wire_id`: the next wire identity to hand out (see below);
//! - `last`: the wire content (wire id → truncated `(x, y)`) of the
//!   **last emitted** snapshot of the room's single group
//!   (`GroupKey = ()`).
//!
//! **Wire identity.** The `entity` field on the wire is *not* the bevy
//! entity bits — it is a room-assigned serial: the `n`-th entity this
//! room ever assigned an identity to (starting at 1), stored in the
//! entity's [`WireId`] component. The counter is monotonic and a value is
//! **never re-used within the room's lifetime**, even when the bevy
//! allocator recycles the old entity's slot. That is what preserves the
//! identity invariant (see `game.proto`): the client's world view is its
//! last accepted snapshot, and an identity present in both the old and
//! the new snapshot is guaranteed to be the *same* entity, so "moved" and
//! "a new entity took the slot" stay distinguishable from the
//! self-contained snapshots alone — including across lost snapshots.
//!
//! The serial is handed out from this room's **single counter** at two
//! call sites: `on_join` (player entities — the same value also goes to
//! the joiner in `JOIN_ROOM_RESULT`, so both paths share one space) and
//! the broadcast pass (everything else that is broadcastable, see
//! below). Both sites go through **one minting point**,
//! [`crate::common::next_serial`], which is the only caller of the
//! crate-private [`WireId::new`]: the counter's space is closed to
//! everything else in the crate, and `WireId`'s private field plus the
//! removed `Default` derive close it to every other crate as well.
//! Bevy's own `(index, generation)` stays internal: its `to_bits()` low
//! half is `0xFFFFFFFF - index`, so the varint was 5 bytes in any
//! realistic room; the serial is 1 byte while the room's total identity
//! count stays below 128 and 2 bytes below 16384.
//!
//! **Broadcastable set: having a [`Position`] is enough.** An entity is
//! broadcast iff it carries a [`Position`], and that precondition is
//! *structural, not a discipline*: entities that have a [`Position`] but
//! no [`WireId`] yet — anything spawned outside `on_join` (bullets,
//! NPCs, traps, …) — are stamped with the next serial **by the broadcast
//! pass itself** and appear in the very snapshot that notices them.
//! Nothing can be silently invisible: before the compact-identity change
//! the broadcast set was exactly "has a `Position`", and this rule
//! restores that contract with the new identity space. The stamp is
//! idempotent (a stamped entity carries a [`WireId`], so it is never
//! stamped again) and costs nothing in steady state (the orphan query
//! matches nothing once every entity is stamped).
//!
//! Broadcasts are **per-group full, self-contained snapshots**: each tick
//! the room asks the logic for one snapshot per group; the logic encodes
//! the group's *entire* world once and reports whether anything changed
//! (including membership — a join/leave changes the set of entities). The
//! room then freezes the payload and shares it by reference with the
//! group's members. No delta, no history: a lost packet is healed by the
//! next snapshot; "nothing changed" stops the emission entirely, and the
//! room's low-rate keep-alive re-sends the cached snapshot so a client
//! that lost its last packet cannot stay stale forever.
//!
//! "No change" compares **exactly what the snapshot carries** — the set
//! of entities and their truncated positions — so the emission decision
//! depends only on wire content: a write that changes a truncated
//! coordinate (or the entity set) is broadcast, and a write that leaves
//! the wire content untouched emits nothing (no band waste). There is no
//! version component and no bump discipline: the content *is* the change
//! signal.
//!
//! `last` is a **single-group** ledger, correct because this room has
//! exactly one group. If you change `GroupKey` to a multi-group key
//! (e.g. `ConnectionId`), you MUST key the ledger by group: the room
//! calls `snapshot()` once per group per tick in unspecified order, and a
//! ledger shared across groups makes the groups visited after the first
//! see "no change" and their members starve (see `RoomLogic::snapshot`).
//!
//! Delta compression and area-of-interest grouping (a non-`()` `GroupKey`)
//! are the documented next steps (see `docs/DESIGN.md`).

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, EntityId};
use gsb_core::room::{Action, RoomLogic, TickCtx};
use gsb_ecs::SystemRunner;
use prost::Message;

use crate::components::{Position, WireId};
use crate::op;

/// The default spawn map half-size (world units): the historical 100×100
/// arena. A room built with it spawns bit-identically to the pre-config
/// `spawn_pos`.
pub const DEFAULT_SPAWN_HALF: f32 = 50.0;

/// The demo room: one moving entity per player, free 2D movement.
pub struct DemoRoom {
    runner: SystemRunner,
    conn_entity: HashMap<ConnectionId, Entity>,
    /// The room's wire-identity counter (see module docs, "Wire identity").
    /// Monotonic; a value is never re-used within the room's lifetime. The
    /// **only** writer is [`crate::common::next_serial`] — the single
    /// minting point for every [`WireId`] this room ever stamps.
    next_wire_id: u64,
    /// Half-size of the square spawn map (see [`spawn_pos`]): entities
    /// spawn uniformly in `[-half, half]²`. Configuration, not a
    /// strategy decision — the demo map has no walls, so the map is as
    /// big as the game wants it (a load profile's "wide map" is just a
    /// large value here; the default keeps the historical 100×100 arena).
    spawn_half: f32,
    /// Wire content of the last emitted snapshot of the room's single
    /// group, as `(wire id → (x, y))` (truncated to the wire's
    /// integer positions). The snapshot is re-emitted when this content
    /// changes — i.e. on any position change **or** membership change
    /// (join/leave), which is the room contract for "no change".
    ///
    /// Single-group by construction (`GroupKey = ()`); a multi-group key
    /// requires the ledger to be keyed by group (see module docs and
    /// `RoomLogic::snapshot`).
    last: HashMap<u64, (i32, i32)>,
    /// Per-connection input sequence state (high-water mark + last ack;
    /// see `crate::common::ingest` / `emit_ack`). Strategy-independent:
    /// every room numbers and acknowledges its clients' input the same
    /// way (the client's prediction reconciliation does not care which
    /// visibility strategy the server picked).
    input: HashMap<ConnectionId, crate::common::InputState>,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `RoomLogic::encoded_records`).
    encoded: u64,
}

impl Default for DemoRoom {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoRoom {
    /// Build the demo room over the default 100×100 arena (bit-identical
    /// spawn distribution to the pre-config rooms).
    pub fn new() -> Self {
        Self::with_spawn_half(DEFAULT_SPAWN_HALF)
    }

    /// Build the demo room over a square spawn map of half-size `half`
    /// (entities spawn uniformly in `[-half, half]²`). The load
    /// generator's `spread` profile pairs this with its home distribution
    /// so spawn points and targets live on the same (possibly "wide")
    /// map.
    pub fn with_spawn_half(half: f32) -> Self {
        Self {
            runner: crate::common::movement_runner(),
            conn_entity: HashMap::new(),
            next_wire_id: 0,
            spawn_half: half.max(1.0),
            last: HashMap::new(),
            input: HashMap::new(),
            encoded: 0,
        }
    }
}

/// Deterministic pseudo-random spawn point in a square arena of half-size
/// `half`, derived from the connection id (stable across room re-joins in
/// the same session). `half = 50` reproduces the historical 100×100 arena
/// exactly: the same 1000×1000 lattice, just scaled. `pub` so the other
/// rooms share the exact same spawn distribution (a fair comparison in
/// the load generator) and the sharded room factory can route a join to
/// the home shard by computing the spawn position's region.
pub fn spawn_pos(conn: ConnectionId, half: f32) -> (f32, f32) {
    // The historical 100×100 lattice, scaled: `half = 50` multiplies by
    // exactly 1.0, so the default is bit-identical to the pre-config
    // formula (a re-derivation like `(h % 1000) * 2 * half / 1000` would
    // double-round and drift by ulps for some ids).
    let h = conn.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let scale = half / 50.0;
    let x = ((h % 1000) as f32 / 10.0 - 50.0) * scale;
    let y = (((h >> 32) % 1000) as f32 / 10.0 - 50.0) * scale;
    (x, y)
}

impl RoomLogic<World> for DemoRoom {
    // One group per room: everyone sees the whole world. (The interface
    // supports finer groupings, e.g. `GroupKey = ConnectionId` — but then
    // the `last` ledger above must be keyed by group; see the module
    // docs and `RoomLogic::snapshot`.)
    type GroupKey = ();

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    fn group_of(&self, _world: &World, _conn: ConnectionId) -> Self::GroupKey {
        Default::default()
    }

    fn snapshot(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        _group: &Self::GroupKey,
        out: &mut bytes::BytesMut,
    ) -> bool {
        // Collect the broadcastable state (wire id, truncated wire
        // position) while the query holds the world borrow.
        let mut current: Vec<(u64, i32, i32)> = Vec::new();
        {
            // Identity assignment (module docs, "Wire identity"): entities
            // with a `Position` but no `WireId` — spawned outside
            // `on_join` (bullets, NPCs, traps, …) — are stamped with the
            // next serial here, so the broadcast set is exactly "has a
            // `Position`" and no entity can be silently invisible (see
            // `common::stamp_orphans` for the two-pass pattern and
            // idempotence). The full query below runs *after* the
            // stamps, so it sees every broadcastable entity exactly once
            // (stamped and pre-stamped alike).
            crate::common::stamp_orphans(&mut self.next_wire_id, world);
            let mut query = world.query::<(&WireId, &Position)>();
            for (wire_id, pos) in query.iter(world) {
                current.push((wire_id.get(), pos.x as i32, pos.y as i32));
            }
        }

        // "No change" = identical wire content: the same set of entities
        // at the same (truncated) positions. A membership change
        // (join/leave) or any position change flips it. The comparison is
        // on exactly what the snapshot carries (see module docs).
        let changed = self.last.len() != current.len()
            || current.iter().any(|(entity, x, y)| {
                self.last
                    .get(entity)
                    .map(|(lx, ly)| *x != *lx || *y != *ly)
                    .unwrap_or(true)
            });
        if !changed {
            return false;
        }

        let mut snap = crate::game::WorldSnapshot {
            sequence: ctx.tick,
            entities: Vec::with_capacity(current.len()),
            removed: Vec::new(),
            cell_exits: Vec::new(),
            delta: false,
        };
        for (entity, x, y) in &current {
            snap.entities.push(crate::game::EntityRecord {
                entity: *entity,
                x: *x,
                y: *y,
            });
        }
        // Encoding into an in-memory buffer cannot fail (no I/O, unbounded
        // capacity); treat a failure as a bug rather than dropping the
        // snapshot.
        snap
            .encode(out)
            .expect("protobuf encode into an in-memory buffer failed");

        self.last.clear();
        for (entity, x, y) in &current {
            self.last.insert(*entity, (*x, *y));
        }
        self.encoded += current.len() as u64;
        true
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> EntityId {
        // Shared spawn path (`common::on_join`): deterministic spawn point
        // (this room's spawn map, see the `spawn_half` field), fresh wire
        // identity through the room's single minting point,
        // connection→entity table update. The same value is returned to
        // the joiner in `JOIN_ROOM_RESULT`, so both paths share one space.
        // No spawn event: membership is expressed by presence in the next
        // snapshot, which now includes the new entity (the join happened in
        // the control phase, before this tick's broadcast).
        crate::common::on_join(
            &mut self.conn_entity,
            &mut self.next_wire_id,
            self.spawn_half,
            world,
            conn,
            &mut self.input,
        )
    }

    fn on_leave(&mut self, world: &mut World, conn: ConnectionId) {
        crate::common::on_leave(&mut self.conn_entity, world, conn, &mut self.input)
    }

    fn ingest(&mut self, world: &mut World, _ctx: &TickCtx, actions: &mut Vec<Action>) {
        crate::common::ingest(&self.conn_entity, world, actions, &mut self.input)
    }

    /// The per-connection input acknowledgment (the group snapshot is
    /// shared; the ack is not — `RoomLogic::private` is the per-connection
    /// seam of the batch, so the ack rides the same delivery as the
    /// snapshot, a few bytes per advanced tick, zero otherwise).
    fn private(
        &mut self,
        _world: &mut World,
        conn: ConnectionId,
        _group: &(),
        out: &mut bytes::BytesMut,
    ) -> bool {
        crate::common::emit_ack(&mut self.input, conn, out)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::common::run_systems(&mut self.runner, world, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use gsb_core::id::RoomId;

    fn ctx1() -> TickCtx {
        TickCtx {
            room: RoomId(1),
            tick: 1,
            dt: Duration::from_secs_f64(1.0 / 30.0),
        }
    }

    /// The "no change" decision compares the wire content: a plain
    /// `Position` write — no version component, no bump discipline — must
    /// still be broadcast whenever it changes a truncated coordinate.
    #[test]
    fn snapshot_emits_on_plain_position_write() {
        let mut world = World::new();
        let mut room = DemoRoom::new();
        let wire_id = room.on_join(&mut world, ConnectionId(1));
        let ctx = ctx1();
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx, &(), &mut out), "join emits");
        assert_eq!(wire_id, 1, "first entity gets wire id 1");

        // The bevy handle is the room's business (conn_entity); the join
        // reply carried the wire id, not the bevy bits.
        let e = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(e).insert(Position { x: 42.0, y: -7.0 });

        let mut out2 = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx, &(), &mut out2),
            "a plain position write must still emit"
        );
        let snap = crate::game::WorldSnapshot::decode(out2.as_ref()).expect("decode");
        assert_eq!(snap.entities.len(), 1);
        assert_eq!(snap.entities[0].x, 42);
        assert_eq!(snap.entities[0].y, -7);
    }

    /// The identity invariant (see `game.proto`, `EntityRecord.entity`):
    /// the client's world view is its **last accepted** snapshot, and an
    /// identity present in both the old and the new snapshot must be the
    /// *same* entity — that is what separates "the same entity moved"
    /// from "a new entity took the slot" when there is no delta, no
    /// history, and no out-of-band remapping message.
    ///
    /// The threat this test pins: the bevy allocator **recycles slots**
    /// (a despawned entity's index is handed back out with a bumped
    /// generation). In bevy 0.19 the allocator keeps freed indices in a
    /// local buffer of 128 before they become reusable, so this test
    /// runs 129 join/leave cycles to force a reuse, then asserts:
    ///
    /// 1. the reuse actually happened (the next join lands on an index a
    ///    previous entity owned) — the test is not vacuous; an
    ///    index-based wire identity would be *indistinguishable* here;
    /// 2. the recycled slot carries a **fresh** wire id never seen
    ///    before — so a client whose accepted view still contains the
    ///    old entity (it lost the leave snapshots) reads the new
    ///    snapshot as "new entity", not "old entity moved".
    #[test]
    fn wire_identity_survives_ecs_slot_reuse() {
        let mut world = World::new();
        let mut room = DemoRoom::new();
        let ctx = ctx1();

        // 129 join/leave cycles: every join gets a fresh wire id, every
        // leave despawns the entity (freeing its bevy slot).
        let mut wire_ids: Vec<u64> = Vec::new();
        let (mut index_128, mut gen_128) = (None, 0u32);
        for i in 1..=129u64 {
            let conn = ConnectionId(i);
            let wire_id = room.on_join(&mut world, conn);
            assert!(
                !wire_ids.contains(&wire_id),
                "wire id {wire_id} handed out twice"
            );
            wire_ids.push(wire_id);
            let e = *room.conn_entity.get(&conn).unwrap();
            if i == 128 {
                index_128 = Some(e.index_u32());
                gen_128 = e.generation().to_bits();
            }
            room.on_leave(&mut world, conn);
        }

        // The next join must recycle a bevy slot (129 frees overflow the
        // 128-slot local free buffer): exactly the condition under which
        // a non-unique wire identity would break the invariant.
        let rejoiner = ConnectionId(1000);
        let wire_id = room.on_join(&mut world, rejoiner);
        let e = *room.conn_entity.get(&rejoiner).unwrap();
        assert_eq!(
            e.index_u32(),
            index_128.expect("recorded above"),
            "bevy slot reuse must have happened for this test to be \
             non-vacuous (an index-based identity would alias here)"
        );
        assert_ne!(
            e.generation().to_bits(),
            gen_128,
            "recycled slot carries a bumped generation"
        );
        assert!(
            !wire_ids.contains(&wire_id),
            "recycled slot must carry a fresh wire id, never a previous one"
        );

        // Client model from `game.proto`: the client's accepted view still
        // holds entity #128 (it lost the leave snapshots). From the new
        // snapshot alone it must classify the record.
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx, &(), &mut out), "join emits");
        let snap = crate::game::WorldSnapshot::decode(out.as_ref()).expect("decode");
        assert_eq!(snap.entities.len(), 1);
        let rec = &snap.entities[0];
        assert_eq!(rec.entity, wire_id, "snapshot carries the fresh wire id");
        let old_view: std::collections::HashSet<u64> = [wire_ids[127]]
            .into_iter()
            .collect();
        assert!(
            !old_view.contains(&rec.entity),
            "the client must see a NEW entity, not entity #128 moving \
             (its old wire id is gone; the recycled slot's new id was \
             never in the client's view)"
        );
        // Note what an index-based identity would have produced here: the
        // record's bevy index equals entity #128's index (asserted above),
        // so a client keyed by index would hit its map and misread the new
        // entity as entity #128 teleporting to a spawn point. The wire id
        // is the field that carries the distinction.
    }

    /// The publishable precondition is structural, not a discipline: an
    /// entity that carries a [`Position`] but never passed through
    /// [`DemoRoom::on_join`] (bullets, NPCs, traps — anything not
    /// player-spawned) must not be *silently invisible*. The broadcast
    /// pass stamps it with a fresh serial and includes it in the very
    /// next snapshot — restoring the pre-compact-identity contract
    /// (broadcast set = "has a `Position`").
    #[test]
    fn entity_spawned_outside_on_join_is_broadcast_with_fresh_wire_id() {
        let mut world = World::new();
        let mut room = DemoRoom::new();
        let ctx = ctx1();

        // Two players through the normal path (wire ids 1 and 2).
        room.on_join(&mut world, ConnectionId(1));
        room.on_join(&mut world, ConnectionId(2));

        // A "bullet" spawned directly into the world — no `on_join`.
        let bullet = world.spawn(Position { x: 7.0, y: -3.0 }).id();
        assert!(
            world.get::<WireId>(bullet).is_none(),
            "precondition: the entity has no wire identity"
        );

        // The next snapshot must include it, with a fresh wire id.
        let mut out = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx, &(), &mut out),
            "a new entity is a wire-content change ⇒ emit"
        );
        let snap = crate::game::WorldSnapshot::decode(out.as_ref()).expect("decode");
        assert_eq!(
            snap.entities.len(),
            3,
            "the orphan must not be silently invisible"
        );
        let rec = snap
            .entities
            .iter()
            .find(|e| e.x == 7 && e.y == -3)
            .expect("the orphan's record");
        assert_eq!(
            rec.entity, 3,
            "it gets the next free serial from the room's single counter \
             (fresh: never handed out before, never re-used)"
        );
        assert!(
            world.get::<WireId>(bullet).is_some(),
            "the entity is stamped (one assignment)"
        );

        // Idempotent: the same wire content emits nothing, and the
        // identity is stable across snapshots.
        let mut out2 = bytes::BytesMut::new();
        assert!(
            !room.snapshot(&mut world, &ctx, &(), &mut out2),
            "unchanged content ⇒ silent (no re-stamp, no re-emit)"
        );
        assert_eq!(
            world.get::<WireId>(bullet).copied().map(WireId::get),
            Some(3),
            "the identity is stable across snapshots"
        );

        // The stamped entity moves: the *same* identity at a new position
        // (a client reads "the same entity moved", not "a new entity").
        world.entity_mut(bullet).insert(Position { x: 9.0, y: -3.0 });
        let mut out3 = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx, &(), &mut out3),
            "movement ⇒ wire content changed ⇒ emit"
        );
        let snap3 = crate::game::WorldSnapshot::decode(out3.as_ref()).expect("decode");
        let rec3 = snap3
            .entities
            .iter()
            .find(|e| e.entity == 3)
            .expect("same identity in the new snapshot");
        assert_eq!((rec3.x, rec3.y), (9, -3));
    }

    /// Identical wire content stays silent (a write that leaves the
    /// wire content untouched emits nothing); a membership change is a
    /// wire-content change and must emit.
    #[test]
    fn snapshot_silent_when_wire_content_unchanged() {
        let mut world = World::new();
        let mut room = DemoRoom::new();
        room.on_join(&mut world, ConnectionId(1));
        let ctx = ctx1();
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx, &(), &mut out), "join emits");

        // A position write with no content change...
        let entity = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        let pos = world
            .entity(entity)
            .get::<Position>()
            .copied()
            .expect("spawned above");
        world.entity_mut(entity).insert(pos);
        let mut out2 = bytes::BytesMut::new();
        assert!(
            !room.snapshot(&mut world, &ctx, &(), &mut out2),
            "write without content change ⇒ no change ⇒ silent"
        );

        // ...and a leave is a wire-content change.
        room.on_leave(&mut world, ConnectionId(1));
        let mut out3 = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx, &(), &mut out3),
            "leave ⇒ wire content changed ⇒ emit"
        );
        let snap = crate::game::WorldSnapshot::decode(out3.as_ref()).expect("decode");
        assert!(snap.entities.is_empty(), "left: empty world snapshot");
    }
}
