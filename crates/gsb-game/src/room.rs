//! [`DemoRoom`]: the demo game's game-logic implementation — the shared
//! contract on [`GameLogic`](gsb_core::room::GameLogic), the room-exclusive
//! request/result seams on [`RoomLogic`].
//!
//! A room owns one bevy [`World`] (exclusively — the room actor is the only
//! borrower) plus a small amount of bookkeeping:
//!
//! - `player_entity`: which entity belongs to which player (keyed by the
//!   STABLE [`gsb_core::PlayerId`] — Faz 2 — so the mapping survives a
//!   resume unchanged);
//! - `next_player_id` / `next_wire_id`: the next stable player identity
//!   and the next wire identity to hand out (see below);
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
//! see "no change" and their members starve (see `GameLogic::snapshot`).
//!
//! Delta compression and area-of-interest grouping (a non-`()` `GroupKey`)
//! are the documented next steps (see `docs/DESIGN.md`).

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, EntityId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, GameLogic, ResumeFound, RoomLogic, TickCtx};
use gsb_core::rpc::RequestDecision;
use gsb_ecs::SystemRunner;
use prost::Message;

use crate::common::ParkEntry;
use crate::components::{MoveTarget, Position, WireId};
use crate::economy::EconomyService;
use crate::op;

/// The default spawn map half-size (world units): the historical 100×100
/// arena. A room built with it spawns bit-identically to the pre-config
/// `spawn_pos`.
pub const DEFAULT_SPAWN_HALF: f32 = 50.0;

/// The demo room: one moving entity per player, free 2D movement.
pub struct DemoRoom {
    runner: SystemRunner,
    /// Player → entity (Faz 2: keyed by the STABLE player identity — the
    /// mapping survives resume unchanged; only a join/leave touches it).
    player_entity: HashMap<PlayerId, Entity>,
    /// The player-identity counter (the demo's minting policy for
    /// [`PlayerId`]): monotonic, never reused within the room's
    /// lifetime. Stability across resume comes from the park ledger
    /// carrying the id, not from re-minting.
    next_player_id: u64,
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
    /// Per-player input sequence state (high-water mark + last ack;
    /// see `crate::common::ingest` / `emit_ack`). Strategy-independent:
    /// every room numbers and acknowledges its clients' input the same
    /// way (the client's prediction reconciliation does not care which
    /// visibility strategy the server picked).
    input: HashMap<PlayerId, crate::common::InputState>,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `GameLogic::encoded_records`).
    encoded: u64,
    /// The economy service handle (the RPC pattern's external-I/O half,
    /// see `crate::economy`); `None` = the room answers `ECONOMY`
    /// requests with a normal rejection ("not configured"). A real
    /// deployment always has one (the platform's economy is the thing
    /// the request is delegated to).
    economy: Option<EconomyService>,
    /// The disconnect-park policy knob (see `crate::common::ParkPolicy`
    /// and RECONNECT §3): how long a dropped transport's hero stays in
    /// the world. Zero = the pre-reconnect despawn-on-disconnect.
    park: crate::common::ParkPolicy,
    /// The demo park ledger (§4: it lives in the LOGIC — the core only
    /// queries it through `resume_lookup`). Identity → parked entity +
    /// bot marker; consumed by a resume, tombstoned by an expiry.
    park_ledger: HashMap<String, ParkEntry>,
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
            player_entity: HashMap::new(),
            next_player_id: 0,
            next_wire_id: 0,
            spawn_half: half.max(1.0),
            last: HashMap::new(),
            input: HashMap::new(),
            encoded: 0,
            economy: None,
            park: crate::common::ParkPolicy::default(),
            park_ledger: HashMap::new(),
        }
    }

    /// Set the disconnect-park grace (RECONNECT §3): a dropped transport
    /// parks its hero for this long before the hold ends (toward the bot
    /// handover). `Duration::ZERO` restores the pre-reconnect despawn
    /// semantics exactly. Builder-style, like [`Self::with_economy`].
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: Duration) -> Self {
        self.park.grace = grace;
        self
    }

    /// Attach the economy service handle (the RPC pattern's external-I/O
    /// half; see `crate::economy`). The room delegates `ECONOMY`
    /// requests to it; the answer arrives on a later tick through the
    /// room's completion channel.
    pub fn with_economy(mut self, economy: EconomyService) -> Self {
        self.economy = Some(economy);
        self
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

// Faz 1 trait split (docs/TRAIT-ARCHITECTURE.md): the shared contract —
// snapshot groups, the tick seam, membership, the reconnect surface —
// implements the `GameLogic` supertrait; the request/result seams stay in
// the `RoomLogic` impl below.
impl GameLogic<World> for DemoRoom {
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

    fn group_of(&self, _world: &World, _player: PlayerId) -> Self::GroupKey {
        Default::default()
    }

    fn snapshot(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        _group: &Self::GroupKey,
        // Single-room execution: no boundary records exist here (the
        // sharded actor folds its border exchange into this same seam).
        _borrowed: &[gsb_core::shard::BorrowedRecord],
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

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> Admission {
        // Shared spawn path (`common::on_join`): deterministic spawn point
        // (this room's spawn map, see the `spawn_half` field), a fresh
        // stable player identity + wire identity through their minting
        // counters, and the player→entity table update. The entity value
        // is also returned to the joiner in `JOIN_ROOM_RESULT`, so both
        // paths share one space. No spawn event: membership is expressed
        // by presence in the next snapshot, which now includes the new
        // entity (the join happened in the control phase, before this
        // tick's broadcast).
        crate::common::on_join(
            &mut self.player_entity,
            &mut self.next_player_id,
            &mut self.next_wire_id,
            self.spawn_half,
            world,
            conn,
            &mut self.input,
        )
    }

    fn on_leave(&mut self, world: &mut World, player: PlayerId) {
        crate::common::on_leave(&mut self.player_entity, world, player, &mut self.input)
    }

    // -- the disconnect policy (docs/RECONNECT.md §3/§5/§9; the hook
    //    bodies are shared with every demo room — see `crate::common`) --

    fn on_disconnect(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        identity: &str,
    ) -> Detach {
        crate::common::park_on_disconnect(
            &self.player_entity,
            player,
            identity,
            &self.park,
            &mut self.park_ledger,
        )
    }

    fn on_detach_expired(
        &mut self,
        _world: &mut World,
        player: PlayerId,
        to: gsb_core::room::ExpireTo,
    ) {
        crate::common::park_on_expire(&mut self.park_ledger, player, to);
    }

    fn resume_lookup(&self, world: &World, identity: &str) -> ResumeFound {
        crate::common::park_lookup(world, &self.park_ledger, identity)
    }

    fn on_resume(
        &mut self,
        _world: &mut World,
        identity: &str,
        _conn: ConnectionId,
        player: PlayerId,
        _entity: EntityId,
    ) {
        // Faz 2 shrink: ledger consume + seq/ack reset only — the
        // player-keyed tables kept their keys across the disconnect.
        crate::common::park_resume(
            &mut self.park_ledger,
            &mut self.input,
            identity,
            player,
        );
    }

    fn ingest(&mut self, world: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        // The bot's synthesized frames ride the SAME list as wire input
        // (RECONNECT §9: "bot = bağlantısız girdi kaynağı" — an input
        // source without a connection): one decode/sequence/move path for
        // both.
        crate::common::synthesize_bot_moves(
            self.park_ledger
                .values()
                .filter(|e| e.bot)
                .map(|e| (e.player, e.entity)),
            world,
            ctx,
            actions,
        );
        crate::common::ingest(&self.player_entity, world, actions, &mut self.input)
    }

    /// The per-connection input acknowledgment (the group snapshot is
    /// shared; the ack is not — `GameLogic::private` is the per-connection
    /// seam of the batch, so the ack rides the same delivery as the
    /// snapshot, a few bytes per advanced tick, zero otherwise).
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
    }
}

// Faz 1 trait split: the room-exclusive seams (RPC + match result).
impl RoomLogic<World> for DemoRoom {
    /// The demo's two request kinds (the RPC pattern's two halves, see
    /// `game.proto`):
    ///
    /// - `ABILITY` (room-local): the answer is computed in this tick —
    ///   a range check against the requester's current position and a
    ///   real world mutation (the entity gets a `MoveTarget` toward the
    ///   requested point) — and returned in the same tick's private
    ///   frame. The same-tick snapshot the requester already receives
    ///   reflects the mutation: the request and its effect share a tick.
    /// - `ECONOMY` (external I/O): the answer needs a round trip to the
    ///   economy service (see `crate::economy`), which the room cannot
    ///   await. The decision is `External` with an owning future; the
    ///   core registers the request as pending, runs the future in a
    ///   worker task, and delivers the answer on a later tick through
    ///   the same private path (the client sees one shape for both).
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
                    return Some(RequestDecision::Reject(
                        "no entity for this connection".into(),
                    ));
                };
                let Ok(he) = world.get_entity(entity) else {
                    return Some(RequestDecision::Reject("entity already gone".into()));
                };
                let Some(pos) = he.get::<Position>().copied() else {
                    return Some(RequestDecision::Reject("entity has no position".into()));
                };
                // Room-local validation (a demo rule: the ability reaches
                // 10 world units). Runs synchronously in this tick.
                let dx = use_msg.x as f32 - pos.x;
                let dy = use_msg.y as f32 - pos.y;
                const RANGE: f32 = 10.0;
                if dx * dx + dy * dy > RANGE * RANGE {
                    return Some(RequestDecision::Reject(format!(
                        "target out of range ({} > {RANGE})",
                        (dx * dx + dy * dy).sqrt()
                    )));
                }
                // The effect: a real mutation, applied in this tick (it
                // rides the same tick's snapshot out to the group).
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
                    return Some(RequestDecision::Reject(
                        "undecodable BuyItem payload".into(),
                    ));
                };
                let Some(economy) = self.economy.clone() else {
                    return Some(RequestDecision::Reject(
                        "economy service not configured".into(),
                    ));
                };
                // The room captures a CLONE of the service handle (a
                // cheap sender clone) — the future owns everything it
                // needs and borrows nothing from the room (see the
                // `RequestDecision::External` contract).
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
            // a normal "no handler" rejection (no waiting on a timeout).
            _ => None,
        }
    }

    /// The demo's match result (the control plane's result seam, feature
    /// A): the room's FINAL snapshot at shutdown — the complete,
    /// self-contained state the game considers "the result" (who was in
    /// the room, where they ended up). Encoded as the ordinary
    /// `WorldSnapshot` message (the platform's adapter decodes it
    /// against the same schema it uses for live snapshots).
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
        let wire_id = room.on_join(&mut world, ConnectionId(1)).entity;
        let ctx = ctx1();
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx, &(), &[], &mut out), "join emits");
        assert_eq!(wire_id, 1, "first entity gets wire id 1");

        // The bevy handle is the room's business (player_entity); the
        // join reply carried the wire id, not the bevy bits.
        let e = *room.player_entity.get(&PlayerId(1)).unwrap();
        world.entity_mut(e).insert(Position { x: 42.0, y: -7.0 });

        let mut out2 = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx, &(), &[], &mut out2),
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
            let wire_id = room.on_join(&mut world, conn).entity;
            assert!(
                !wire_ids.contains(&wire_id),
                "wire id {wire_id} handed out twice"
            );
            wire_ids.push(wire_id);
            // The n-th sequential join owns PlayerId(n) (the counter).
            let e = *room.player_entity.get(&PlayerId(i)).unwrap();
            if i == 128 {
                index_128 = Some(e.index_u32());
                gen_128 = e.generation().to_bits();
            }
            room.on_leave(&mut world, PlayerId(i));
        }

        // The next join must recycle a bevy slot (129 frees overflow the
        // 128-slot local free buffer): exactly the condition under which
        // a non-unique wire identity would break the invariant.
        let rejoiner = ConnectionId(1000);
        let wire_id = room.on_join(&mut world, rejoiner).entity;
        // The 130th sequential join owns PlayerId(130).
        let e = *room.player_entity.get(&PlayerId(130)).unwrap();
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
        assert!(room.snapshot(&mut world, &ctx, &(), &[], &mut out), "join emits");
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
            room.snapshot(&mut world, &ctx, &(), &[], &mut out),
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
            !room.snapshot(&mut world, &ctx, &(), &[], &mut out2),
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
            room.snapshot(&mut world, &ctx, &(), &[], &mut out3),
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
        assert!(room.snapshot(&mut world, &ctx, &(), &[], &mut out), "join emits");

        // A position write with no content change...
        let entity = *room.player_entity.get(&PlayerId(1)).unwrap();
        let pos = world
            .entity(entity)
            .get::<Position>()
            .copied()
            .expect("spawned above");
        world.entity_mut(entity).insert(pos);
        let mut out2 = bytes::BytesMut::new();
        assert!(
            !room.snapshot(&mut world, &ctx, &(), &[], &mut out2),
            "write without content change ⇒ no change ⇒ silent"
        );

        // ...and a leave is a wire-content change.
        room.on_leave(&mut world, PlayerId(1));
        let mut out3 = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx, &(), &[], &mut out3),
            "leave ⇒ wire content changed ⇒ emit"
        );
        let snap = crate::game::WorldSnapshot::decode(out3.as_ref()).expect("decode");
        assert!(snap.entities.is_empty(), "left: empty world snapshot");
    }
}
