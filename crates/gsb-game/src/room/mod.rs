//! [`OpenRoom`]: the demo game's game-logic implementation — the shared
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

mod logic;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::Entity;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_ecs::SystemRunner;

use crate::common::ParkEntry;
use crate::economy::EconomyService;

/// The default spawn map half-size (world units): the historical 100×100
/// arena. A room built with it spawns bit-identically to the pre-config
/// `spawn_pos`.
pub const DEFAULT_SPAWN_HALF: f32 = 50.0;

/// The open-visibility strategy room (`GroupKey = ()`): one moving entity
/// per player, free 2D movement — everyone sees everything (the
/// unrestricted baseline the restricted strategies are measured against).
pub struct OpenRoom {
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
    /// see `crate::common::ingest` / `emit_private`). Strategy-independent:
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

impl Default for OpenRoom {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenRoom {
    /// Build the open room over the default 100×100 arena (bit-identical
    /// spawn distribution to the pre-config rooms).
    pub fn new() -> Self {
        Self::with_spawn_half(DEFAULT_SPAWN_HALF)
    }

    /// Build the open room over a square spawn map of half-size `half`
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
