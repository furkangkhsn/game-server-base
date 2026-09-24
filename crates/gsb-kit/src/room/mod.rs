//! [`OpenRoom`]: the open-visibility strategy, generic over the game
//! ([`Game`], KIT-ARCHITECTURE §4.3) — the shared contract on
//! [`GameLogic`](gsb_core::room::GameLogic), the room-exclusive
//! request/result seams on
//! [`RoomLogic`](gsb_core::room::RoomLogic).
//!
//! A room owns one bevy [`World`] (exclusively — the room actor is the only
//! borrower) plus a small amount of bookkeeping:
//!
//! - `player_entity`: which entity belongs to which player (keyed by the
//!   STABLE [`gsb_core::PlayerId`] — Faz 2 — so the mapping survives a
//!   resume unchanged);
//! - `next_player_id` / `minter`: the next stable player identity
//!   and the next wire identity to hand out (see below);
//! - `last`: the wire content (wire id → the codec's wire value) of the
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
//! below). Both sites go through **one minting point**, the room's
//! [`crate::identity::Minter`] — the only code that can construct a
//! [`WireId`] at all (its field is private to the identity module, it has
//! no constructor and no `Default`): the counter's space is closed to
//! everything else in this crate and in every other crate.
//! Bevy's own `(index, generation)` stays internal: its `to_bits()` low
//! half is `0xFFFFFFFF - index`, so the varint was 5 bytes in any
//! realistic room; the serial is 1 byte while the room's total identity
//! count stays below 128 and 2 bytes below 16384.
//!
//! **Broadcastable set: having the codec's marker is enough** (the demo:
//! a `Position`). An entity is broadcast iff it carries the
//! [`RecordCodec::Marker`](crate::codec::RecordCodec::Marker), and
//! that precondition is *structural, not a discipline*: entities that
//! have the marker but no [`WireId`] yet — anything spawned outside `on_join` (bullets,
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
//! of entities and their wire values (the demo: truncated positions) —
//! so the emission decision depends only on wire content: a write that
//! changes a wire value (or the entity set) is broadcast, and a write that leaves
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
use gsb_core::id::PlayerId;

use crate::common::{InputSeq, ParkEntry, ParkPolicy};
use crate::game::{Game, Wire};
use crate::identity::Minter;

/// The open-visibility strategy room (`GroupKey = ()`): everyone sees
/// everything (the unrestricted baseline the restricted strategies are
/// measured against). Generic over the game `G` (KIT-ARCHITECTURE §4.3):
/// the room keeps the tables, the identities and the snapshot envelope;
/// the game spawns, decodes, simulates and encodes the records.
pub struct OpenRoom<G: Game> {
    /// The game (its hooks and its own state — the demo's system stack,
    /// spawn map, economy handle).
    game: G,
    /// Player → entity (Faz 2: keyed by the STABLE player identity — the
    /// mapping survives resume unchanged; only a join/leave touches it).
    player_entity: HashMap<PlayerId, Entity>,
    /// The player-identity counter (the room's minting policy for
    /// [`PlayerId`]): monotonic, never reused within the room's
    /// lifetime. Stability across resume comes from the park ledger
    /// carrying the id, not from re-minting.
    next_player_id: u64,
    /// The room's wire-identity counter (see module docs, "Wire identity"):
    /// the single minting point for every
    /// [`WireId`](crate::identity::WireId) this room ever stamps
    /// ([`Minter`] — the only construction path of the type). Monotonic;
    /// a value is never re-used within the room's lifetime.
    minter: Minter,
    /// Wire content of the last emitted snapshot of the room's single
    /// group, as `(wire id → wire value)` (the codec's quantized
    /// `Wire`). The snapshot is re-emitted when this content changes —
    /// i.e. on any record change **or** membership change (join/leave),
    /// which is the room contract for "no change".
    ///
    /// Single-group by construction (`GroupKey = ()`); a multi-group key
    /// requires the ledger to be keyed by group (see module docs and
    /// `RoomLogic::snapshot`).
    last: HashMap<u64, Wire<G>>,
    /// Per-player input sequence state (high-water mark + last ack; see
    /// `crate::common::emit_private`). Strategy-independent: every
    /// room numbers and acknowledges its clients' input the same way.
    input: InputSeq,
    /// Entity records encoded during the most recent broadcast phase
    /// (polled by the room via `GameLogic::encoded_records`).
    encoded: u64,
    /// The disconnect-park policy knob (see `crate::common::ParkPolicy`
    /// and RECONNECT §3): how long a dropped transport's hero stays in
    /// the world. Zero = the pre-reconnect despawn-on-disconnect.
    park: ParkPolicy,
    /// The park ledger (§4: it lives in the LOGIC — the core only
    /// queries it through `resume_lookup`). Identity → parked entity +
    /// bot marker; consumed by a resume, tombstoned by an expiry.
    park_ledger: HashMap<String, ParkEntry>,
}

impl<G: Game> OpenRoom<G> {
    /// Build the open room around `game`.
    pub fn with_game(game: G) -> Self {
        Self {
            game,
            player_entity: HashMap::new(),
            next_player_id: 0,
            minter: Minter::sequential(),
            last: HashMap::new(),
            input: InputSeq::default(),
            encoded: 0,
            park: ParkPolicy::default(),
            park_ledger: HashMap::new(),
        }
    }

    /// Set the disconnect-park grace (RECONNECT §3): a dropped transport
    /// parks its hero for this long before the hold ends (toward the bot
    /// handover). `Duration::ZERO` restores the pre-reconnect despawn
    /// semantics exactly. Builder-style.
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: Duration) -> Self {
        self.park.grace = grace;
        self
    }

    /// The game this room runs.
    pub fn game(&self) -> &G {
        &self.game
    }

    /// The game this room runs, for configuration after construction
    /// (e.g. attaching a service handle the game's requests delegate to).
    pub fn game_mut(&mut self) -> &mut G {
        &mut self.game
    }
}

// Faz 1 trait split (docs/TRAIT-ARCHITECTURE.md): the shared contract —
// snapshot groups, the tick seam, membership, the reconnect surface —
// implements the `GameLogic` supertrait; the request/result seams stay in
// the `RoomLogic` impl below.
