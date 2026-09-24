//! The Faz B composite: the same N-actor grid, but each shard
//! broadcasts with cell-grouped spatial AOI over its own region.

use std::collections::{HashMap, HashSet};

use gsb_core::id::PlayerId;
use gsb_core::room::TickCtx;
use gsb_core::shard::BorderRecord;

use crate::economy::EconomyService;
use crate::kit::common::{Cell, CellBook, CellPieces, cell_of};
use crate::sharded::*;

mod logic;
mod shard;

/// The `sharded × spatial` composite (ROADMAP Faz B — see the module
/// docs, "The spatial composite"): the grid topology of
/// [`ShardedRoom`] with each shard's broadcast phase re-grouped by
/// spatial cell and delta-encoded against last-sent content — AoiRoom's
/// engine ([`crate::kit::common::CellBook`] / [`crate::kit::common::CellPieces`])
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
    pub(in crate::sharded) inner: ShardedRoom,
    /// World units per cell edge (the config's `aoi_cell_size`; the same
    /// knob the single-world AOI room turns).
    pub(in crate::sharded) cell_size: f32,
    /// The content bookkeeping shared with [`crate::aoi::AoiRoom`] —
    /// buckets over OWN entities AND borrowed records alike, change
    /// lists, member counts, born groups. Fed from two sources: the
    /// bevy dirty pass in `update` (own entities) and
    /// [`Self::integrate_borrowed`] (the strip diff).
    pub(in crate::sharded) book: CellBook,
    /// THE ledger (module docs, "THE borrowed-strip × delta-ledger
    /// subtlety"): the previous tick's flattened borrowed view,
    /// `wire → (x, y)` truncated. The new slice is diffed against THIS,
    /// never against the buckets, so an unchanged strip dirties nothing.
    pub(in crate::sharded) prev_borrowed: HashMap<u64, (i32, i32)>,
    /// Once-per-tick guard for the strip integration + deferred roll:
    /// the tick whose broadcast-phase preparation has already run.
    pub(in crate::sharded) integrated_tick: u64,
    /// Per-player view baseline (`player → the cell whose FULL view was
    /// last delivered to it`): missing/other ⇒ the one-shot private
    /// full. Cleared on join/resume/migrate-in/migrate-out — a fresh
    /// session or a fresh shard MUST re-baseline (module docs, "Migration
    /// correctness").
    pub(in crate::sharded) conn_view: HashMap<PlayerId, Cell>,
    /// The global tick of the current step (set in `update`).
    pub(in crate::sharded) tick: u64,
    // ── Per-tick piece caches (cleared in `update`, computed lazily in
    //    the broadcast phase; order-independent across groups). ──
    pub(in crate::sharded) pieces: CellPieces,
    /// The groups that emitted a FULL this tick (fresh group /
    /// keepalive): their members' private frames skip the one-shot.
    pub(in crate::sharded) group_full_emitted: HashSet<Cell>,
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
        let mut new_view: HashMap<u64, (i32, i32)> = HashMap::with_capacity(borrowed.len());
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
                    self.book
                        .record_appearance(rec.wire, rec.state.x, rec.state.y, c, false);
                }
                Some(prev) if prev != pos => {
                    // Moved: one upsert — or exit+upsert when the move
                    // crossed a cell boundary (the packet passes fix the
                    // wire order).
                    let old_c = cell_of(prev.0, prev.1, self.cell_size);
                    let new_c = cell_of(rec.state.x, rec.state.y, self.cell_size);
                    if old_c == new_c {
                        self.book
                            .record_update(new_c, rec.wire, rec.state.x, rec.state.y);
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
