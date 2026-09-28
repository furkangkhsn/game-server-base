//! [`AoiRoom`]: an Area-of-Interest (AOI) game logic — in its
//! **cell-encoded delta** form — generic over the game (`G: Game`,
//! KIT-ARCHITECTURE §4.3) and the cell space (`S: CellSpace<Wire<G>>`,
//! §4.2). The sections below speak in the terms of the demo's
//! instantiation (`DemoGame` + the kit's `Grid2` preset: `Position`
//! records, a 3×3 block of `cell_size` cells); for another game read
//! "the codec's `Dirty` filter" for `Changed<Position>`, "the space's
//! view" for the 3×3 block.
//!
//! ## Encoding unit vs audience (the design, decided — see the module
//! docs' "Why this shape")
//!
//! The snapshot group key is a **spatial cell** (`GroupKey = Cell`), but
//! the *encoding* no longer happens per group's 3×3 block:
//!
//! - **A cell is the encoding unit.** Each cell encodes its own content
//!   **once per tick** (a *full piece* — all its records — or a *delta
//!   piece* — its exits and changed records against the previous tick).
//!   The pieces are frozen `Bytes`, shared by reference with every group
//!   that needs them (Arc refcount — the same "compute once, share the
//!   bytes" spine, now at cell granularity instead of block granularity).
//! - **A group is the audience.** A group's (cell's) packet is the
//!   *concatenation* of the pieces of the cells in its 3×3 neighborhood
//!   (pre-encoded blocks concatenate into valid protobuf: the repeated
//!   fields simply continue — verified end to end, see `tests/delta_aoi.rs`).
//! - **The full/delta decision is per (group, cell) pair**, and with a
//!   fixed 3×3 neighborhood it reduces to the cell's own transition plus
//!   the group's age:
//!   - a **fresh** group (it had no members last tick) emits a **full**
//!     packet on its first tick (`delta = false`): all its members are new
//!     to the view and have no baseline;
//!   - for an **established** group the packet is a **delta**
//!     (`delta = true`), and per cell: *appeared* (was empty, now has
//!     content) → the cell's **full** records as upserts (the client has
//!     no baseline for that cell); *continues* (content changed) → the
//!     cell's **delta** piece (exits, then updates); *became empty* → a
//!     single **`CellExit` record** (the client forgets every entity it
//!     holds for that cell — one record per cell, whatever the cell's
//!     population); *silent* → nothing.
//!
//! That collapses two historical costs into one change: the encoding is
//! **E records per tick** (each entity is encoded once, in its own
//! cell's piece) instead of ~overlap×E (each entity encoded into every
//! overlapping 3×3 block — the measured overlap was 8.28 for the dense
//! profile), and the delta is not a separate feature: it is what
//! "encode the cell, not the block, against the previous tick" already
//! produces.
//!
//! ## The delta invariant (why the piece can be shared across groups)
//!
//! A cell's delta is `content(now) vs content(previous tick)` — a pure
//! per-cell function, independent of the group. That is only sound if
//! every established group's clients are always in sync with the
//! *previous tick's* content of every cell in their view. The induction:
//! on an emitting tick the group's packet brings every visible cell to
//! the current content (full records, or the exit/update diff against
//! exactly the previous content); on a silent tick the group is silent
//! *because* every visible cell's content equals what its clients hold.
//! Hence, at any tick, an established group's client state for a visible
//! cell equals the previous tick's content of that cell — and the next
//! delta is computed against exactly that. No per-group ledgers are
//! needed at all: the room keeps the current buckets (incrementally
//! maintained — "Dirty cells" below), the per-tick change lists, and,
//! per group, the fact that it was born.
//!
//! **With a send rate** (a game's opt-in, `RecordCodec::send_every` —
//! KIT-ARCHITECTURE §10 "A10") the induction holds per record instead of
//! per value: a record whose value changed inside its cell but is not
//! due yet stays out of the change list (its clients keep its last sent
//! value; the bucket, and so every full, has the current one) and joins
//! the list on its due step with its current value. The deferral is the
//! record's, never a group's, so every established group still holds the
//! same state of the cell and the piece is still shared. Appearing,
//! crossing and exiting never wait.
//!
//! The invariant is a server-side one (the per-cell delta is group-
//! independent no matter what the clients hold). A client that is loss-
//! free is exactly in sync with the previous tick's content at every
//! tick; a client that lost frames is not (its state lags). Such a
//! client applies every delta it receives **on top of its stale state,
//! best effort**: the records are absolute (upserts of current
//! positions, forgets by absolute wire id / cell) and idempotent, so
//! misapplication is impossible — the worst case is a stale view (a
//! missed exit may ghost briefly). Exact resync comes from the **full**
//! the server ships on the keep-alive cadence (see
//! the shared contract on `GameLogic` ([`GameLogic::keepalive`](gsb_core::room::GameLogic::keepalive) is the
//! recovery path) and one-shot via [`GameLogic::private`](gsb_core::room::GameLogic::private) to
//! every fresh group member (see below); recovery is bounded by the
//! keep-alive period. Note a sequence gap is therefore NOT proof of
//! loss — the group's stream is event-driven (a group ships a frame
//! only when its content changes; wire positions are integer-quantized
//! while motion is fractional, so even a moving entity is silent for a
//! run of ticks) — the full is the guarantee, not the gap.
//!
//! ## Dirty cells (per-tick work ∝ movers, not entities)
//!
//! The delta is **not** computed by diffing this tick's buckets against
//! last tick's: the room keeps only the current buckets (maintained
//! **incrementally**) plus, per tick, each *touched* cell's **change
//! list** — the exits and updates that happened to it. The change list
//! *is* the diff, so a cell's delta piece is assembled from it directly
//! and no per-cell content comparison is ever run (diffing was
//! O(occupied cells × cell size) per tick; assembling is O(changed
//! records)).
//!
//! **The dirty marking is structural, not a discipline.** A cell is
//! dirty exactly when some entity's `Position` was written this tick —
//! and that fact is recorded by **bevy's own change detection**
//! (`Changed<Position>`), not by the room calling a `bump()` method:
//! the mark is set inside bevy's write path, so *any* writer — the
//! movement system, `ingest`, a direct `world.entity_mut` in a test,
//! future game code — marks the cell dirty by construction. This is the
//! same shape as the `WireId` fix (private field, single mint point):
//! the previous design's comment-only "writers must remember to
//! invalidate" discipline was bitten three times in this project
//! (see `docs/ROADMAP.md`), so the mark now lives where it cannot be
//! forgotten. `DESIGN.md` §7 avoided bevy's *observer/event* change API
//! for the room's input path; standalone `Changed<T>` with a manual
//! `World::clear_trackers()` at the end of `update` is the same
//! mechanism used through its query interface — the baseline is the end
//! of the previous `update`, so the query window covers the systems'
//! writes, between-update spawns (joins), and direct writes alike
//! (verified by `tests/zzz_probe_bevey.rs` during design; probe deleted,
//! behaviour locked by the change-detection tests in this module).
//!
//! What the query window *does not* cover — and how each case is
//! handled:
//!
//! - **Despawns are not writes.** `on_leave` despawns the entity in the
//!   CONTROL phase, so the query (which iterates live entities) cannot
//!   see it: the leave parks the entity (a member) in
//!   `pending_removals`, which `update` applies against the buckets
//!   (its wire id and the cell it was in are read from `last_cell`,
//!   written in `update` and nowhere else). A join+leave within one
//!   tick parks nothing — the entity never made it into `last_cell`,
//!   hence never into the buckets. A despawn nobody parked — the GAME
//!   despawning an NPC — is read from the world's removed-component
//!   buffers in `update`, before the tick's one change-window close
//!   (KIT-ARCHITECTURE §8.2).
//! - **Quantization.** Wire positions are i32 truncations of f32 motion:
//!   a sub-cell move can leave the wire position untouched. The dirty
//!   query still flags the write, and the same-cell branch compares the
//!   OLD wire record against the new one — an unchanged wire position
//!   records nothing (the cell can still classify `Silent`), so the
//!   stream is content-identical to the diff-based design.
//! - **Same-value rewrites** are tracked by bevy (every write dirties,
//!   even an identical one) and degrade to the no-op above.
//!
//! **Occupancy and birth, order-independently.** A cell's appeared/exited
//! flag is a pure function of (occupied at the end of the last `update`,
//! occupied now): `update` keeps `prev_occupied` **frozen** while the
//! dirty loop runs (it is rolled only afterwards, per touched cell, from
//! the final bucket state), so same-tick exit+entry into the same cell
//! cannot flip either flag. Group birth (a member count going 0 → >0)
//! is reconstructed from the net member events —
//! `before = now − in + out`, evaluated per touched cell after all
//! events — so a same-tick member exit and a different member's entry
//! into the same cell cannot fake a birth. The per-tick passes (dirty
//! loop, flag/birth roll) iterate the touched cells / change lists,
//! i.e. O(movers) — never the occupied cells or the entity count.
//! (An earlier literal reading of the birth rule as "cells with members
//! now minus cells with members last tick" would miss a member joining a
//! cell that already held non-member content — the cell was in both
//! sets; the member-count formulation is the correct generalization, and
//! in the member-only case it coincides with the literal reading. The
//! missed-birth consequence is observational only: the new member is
//! baselined by its one-shot private full in the same batch, see
//! "Late joiners".)
//!
//! ## Ordering inside a delta packet
//!
//! The layout is fixed and part of the protocol (`game.proto`):
//! `[header: sequence, delta][removed: entity exits][cell_exits][entities:
//! updates]`. An entity moving from cell X to cell Y is *exited* from X
//! (X's delta piece reports its own lost membership — cell-local and
//! cheap) and *updated* in Y; a client that sees both cells must process
//! the exit before the update (it does: exits come first), a client that
//! sees only X sees the exit (the entity correctly vanishes from its
//! view), one that sees only Y sees the update (it correctly appears).
//!
//! ## Late joiners and group crossings (the one-shot private full)
//!
//! A connection that (re)joins a group, or crosses into a new cell's
//! group, has **no baseline** for that group's view — a delta has nothing
//! to apply against ("what changed" is meaningless without "what is
//! there"). It therefore receives a one-shot **full** of its new group's
//! 3×3 via the per-connection `private` frame (once per join/crossing),
//! and the group stays in delta mode for everyone (no per-connection
//! groups). If the group's own emission this tick is already a full
//! (a fresh group's first packet, or a keep-alive full), that frame is
//! in the same batch ahead of the private frame and the one-shot is
//! skipped. This is the AOI form of the `late_joiner_receives_full_world_
//! snapshot` guarantee.
//!
//! ## Keep-alive (the delta's recovery path)
//!
//! Re-sending the group's last payload on a keep-alive tick is
//! meaningless in delta mode: the last payload is a *delta* — a client
//! that missed it has no baseline to apply it to, and a current client
//! would double-apply it. The keep-alive tick therefore ships a **fresh
//! full** for the group (encoded from the cells' full pieces — shared
//! across groups, cached per tick), *whether the group is active or
//! silent*: on an active group's tick the full replaces that tick's
//! delta (a superset — healthy clients simply take the full), and a
//! client that lost one or more deltas is healed within one keep-alive
//! **period** (≤ 1 s + 1 tick at the defaults) regardless of how busy
//! its group is. For a silent group the full's content equals what
//! healthy clients already hold (the delta invariant), so it is a no-op
//! for them. The cost is one full *encode* per group per keep-alive tick
//! (≈ the bytes the old keep-alive re-sent, once per second).
//!
//! ## Security parameter: the visible cell set
//!
//! The group's visible cell set is its **3×3 neighborhood, content-
//! agnostic and omnidirectional**: a cell contributes to the group's
//! packet iff it is in the neighborhood; empty cells contribute nothing
//! (nothing leaks from an empty cell). The visibility boundary is
//! quantized to cells: an entity is broadcast iff its cell is in the
//! group's 3×3, i.e. the error band is at most one cell edge beyond the
//! group cell's boundary (≤ `cell_size` world units; worst case ~2×
//! `cell_size` from a member at the group cell's far edge). The
//! omnidirectional block is the price of the shared-bytes model: a finer
//! "lit cells" notion (per-player facing/vision) makes the snapshot a
//! function of the *player*, not the *cell*, which one shared packet per
//! cell cannot express — a game that needs it opts into
//! [`LitAoiRoom`] (A9: a viewer with a light is its own group, its
//! frames carry only what its game lights; everyone else keeps the
//! shared packets). In this room `cell_size` is the single knob that
//! sets both the concealment resolution and the leak band (see
//! `docs/DESIGN.md` §8).
//!
//! **Why a block keyed on the cell, and not a per-player radius?** A
//! per-player radius would make the snapshot a function of the *player*,
//! not the *cell* — every player in a cell would need a different
//! payload, which the group-snapshot model (one `Bytes` per group,
//! shared by reference) cannot express without per-connection groups or
//! a core change. A block keyed on the cell keeps the "compute once per
//! cell, share the `Bytes`" property exactly. (Alternatives rejected in
//! the original AOI turn stand: own-cell-only — the boundary blind spot;
//! larger blocks — ~2.8× the encoding cost and bigger payloads.)
//!
//! ## Cell size
//!
//! `cell_size` (world units per cell edge) is the one tunable: it must
//! keep a cell's 3×3 block under `max_snapshot_bytes` at the expected
//! *peak* entity density, and it sets the leak band (above).
//!
//! ## Cells are computed from the WIRE position
//!
//! The cell of an entity is `floor(wire_x / cell_size)` on the
//! **integer** (wire) coordinates, not the f32 simulation position: the
//! client holds only wire coordinates and must compute the *same* cell
//! to service `CellExit` records (forget every entity it holds for the
//! exited cell). Server and client therefore bucket by the truncated
//! position; the group key (`group_of`) uses the same formula, so a
//! connection's group is exactly the cell its entity's records land in.
//!
//! ## Invariants preserved (see `tests/aoi.rs`, `tests/delta_aoi.rs`)
//!
//! - **Identity**: the wire id is minted once and never changes; a
//!   cell-changing entity keeps it.
//! - **Late join / group crossing**: a fresh group member sees its full
//!   visibility block (one-shot private full, or the group's own fresh
//!   full in the same batch).
//! - **Broadcast set**: exactly "has a `Position`" (orphan stamping in
//!   `update`, structural like `OpenRoom`).
//! - **Convergence**: the delta stream and the full stream produce the
//!   same client view of the same world (two paths, one result).
//! - **No ghosts, no duplicates**: an entity in a cell the client cannot
//!   see is neither drawn nor duplicated; exits (per entity, per cell)
//!   are delivered on the group's packet.

mod lit;
mod logic;

#[cfg(test)]
mod tests;

pub use lit::{LitAoiRoom, LitGroup};

use std::collections::{HashMap, HashSet};

use bevy_ecs::prelude::Entity;
use gsb_core::id::PlayerId;

use crate::codec::RecordCodec;
use crate::common::{
    Baselines, Cached, CellBook, CellPieces, DirtyPass, InputSeq, Orphans, ParkEntry, ParkPolicy,
};
use crate::game::{Game, Wire};
use crate::identity::Minter;
use crate::space::CellSpace;
// The 2D grid preset's cell key, in scope for the in-module tests (they
// drive the fixture's `Grid2` instantiation and name its cells through
// `use super::*`).
#[cfg(test)]
use crate::space::Cell;

/// The AOI room: spatial group key (audience), per-cell encoding (unit),
/// per-cell delta against the previous tick, one-shot private fulls for
/// fresh group members, keep-alive fulls as the loss-recovery path.
/// The cell-delta engine itself (`crate::common::CellBook` /
/// `crate::common::CellPieces`) is shared with the sharded spatial
/// composite; this room contributes only the session surface and the
/// single-world feeding of the bookkeeping.
pub struct AoiRoom<G: Game, S: CellSpace<Wire<G>>> {
    /// The game (its hooks, its codec and its own state).
    game: G,
    /// The cell space (the demo: `Grid2` — world units per cell edge;
    /// see module docs, "Cell size" — it also sets the leak band,
    /// "Security parameter").
    space: S,
    /// Which entity belongs to which player (Faz 2: keyed by the STABLE
    /// player identity — the mapping survives resume unchanged).
    player_entity: HashMap<PlayerId, Entity>,
    /// The player-identity counter (the room's [`PlayerId`] minting
    /// policy); monotonic, never reused within the room's lifetime.
    next_player_id: u64,
    /// The disconnect-park policy + ledger (see `crate::common` and
    /// RECONNECT §3/§9; the hook bodies are shared with every room).
    park: ParkPolicy,
    park_ledger: HashMap<String, ParkEntry>,
    /// The room's single wire-identity counter (see module docs,
    /// "Invariants preserved" / `game.proto`).
    minter: Minter,
    /// Per-player input sequence state (strategy-independent; see
    /// `crate::common::emit_private`).
    input: InputSeq,
    /// Per-PLAYER view baseline: `player → the cell whose FULL view was
    /// last delivered to it` (via the one-shot private full, or via the
    /// group's own full in the same batch). A player whose entry is
    /// missing or names another cell has no baseline for its current
    /// group's view and gets a one-shot private full (see `private`).
    /// SESSION-scoped content under a stable key: a resume clears it
    /// (`on_resume`) so the fresh session re-baselines with a full; a
    /// fan-out drop of the batch that carried view content takes it
    /// back, paced (`Baselines` — F11).
    baselines: Baselines<S::Cell>,
    /// The content bookkeeping (buckets, change lists, occupancy and
    /// member baselines, parked removals, born groups) — the shared
    /// engine ([`crate::common::CellBook`]); this room feeds it from the
    /// bevy dirty query alone (no borrowed strip exists here).
    book: CellBook<Wire<G>, S::Cell>,
    /// The global tick of the current step (set in `update`): the
    /// `private` seam has no `TickCtx`, so the tick it stamps into
    /// payloads comes from here.
    tick: u64,
    // ── Per-tick caches (cleared in `update`; computed lazily in the
    //    broadcast phase — the room calls `snapshot`/`keepalive`/
    //    `private` once per group/conn in unspecified order, and the
    //    cache makes the pieces order-independent: the same (cell, kind)
    //    is computed once, shared as frozen `Bytes` by reference). ──
    pieces: CellPieces<S::Cell>,
    /// The groups that emitted a FULL this tick (a fresh group in
    /// `snapshot`, a silent group in `keepalive`): a member of such a
    /// group is baselined by that frame (it precedes the private frame in
    /// the batch), so `private` skips its one-shot full.
    group_full_emitted: HashSet<S::Cell>,
    /// The dirty pass's query and the orphan query, kept across ticks
    /// (`crate::common::Cached`, A12).
    dirty: DirtyPass<G::Codec>,
    orphans: Orphans<<G::Codec as RecordCodec>::Marker>,
}

impl<G: Game, S: CellSpace<Wire<G>>> AoiRoom<G, S> {
    /// Build an AOI room running `game` over the cell space `space`.
    #[must_use]
    pub fn with_game(game: G, space: S) -> Self {
        Self {
            game,
            space,
            player_entity: HashMap::new(),
            next_player_id: 0,
            park: ParkPolicy::default(),
            park_ledger: HashMap::new(),
            minter: Minter::sequential(),
            input: InputSeq::default(),
            baselines: Baselines::default(),
            book: CellBook::default(),
            tick: 0,
            pieces: CellPieces::default(),
            group_full_emitted: HashSet::new(),
            dirty: Cached::default(),
            orphans: Cached::default(),
        }
    }

    /// Set the disconnect-park grace (see
    /// [`crate::room::OpenRoom::with_disconnect_grace`]; RECONNECT §3).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.park.grace = Some(grace);
        self
    }

    /// Set the whole disconnect-park policy (see
    /// [`crate::room::OpenRoom::with_disconnect_policy`]; RECONNECT
    /// §3/§14.4).
    #[must_use]
    pub fn with_disconnect_policy(
        mut self,
        grace: Option<std::time::Duration>,
        to: gsb_core::room::ExpireTo,
    ) -> Self {
        self.park.grace = grace;
        self.park.to = to;
        self
    }

    /// Override the disconnect policy for one cause (see
    /// [`crate::room::OpenRoom::with_disconnect_policy_for`]; BACKLOG
    /// F27).
    #[must_use]
    pub fn with_disconnect_policy_for(
        mut self,
        cause: gsb_core::room::DisconnectCause,
        grace: Option<std::time::Duration>,
        to: gsb_core::room::ExpireTo,
    ) -> Self {
        self.park.set_for(cause, grace, to);
        self
    }

    /// The game this room runs.
    pub fn game(&self) -> &G {
        &self.game
    }

    /// The game this room runs, for configuration after construction.
    pub fn game_mut(&mut self) -> &mut G {
        &mut self.game
    }
}

// Faz 1 trait split: this room implements only shared hooks, so its
// whole surface lives on the `GameLogic` supertrait; the `RoomLogic`
// impl below stays empty (both exclusive methods have defaults).
