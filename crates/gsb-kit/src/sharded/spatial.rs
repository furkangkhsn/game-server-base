//! The Faz B composite: the same N-actor grid, but each shard
//! broadcasts with cell-grouped spatial AOI over its own region.

use std::collections::{HashMap, HashSet};

use gsb_core::id::PlayerId;
use gsb_core::room::TickCtx;
use gsb_core::shard::BorderRecord;

use crate::common::{CellBook, CellPieces};
use crate::game::{ShardGame, Wire};
use crate::sharded::*;
use crate::space::{CellSpace, Partition};

mod logic;
mod shard;

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
pub struct ShardedSpatialRoom<G: ShardGame, P: Partition<Wire<G>>, S: CellSpace<Wire<G>>> {
    /// The grid-protocol half (delegated hooks; same module, so its
    /// private tables are readable where the seam requires it).
    pub(in crate::sharded) inner: ShardedRoom<G, P>,
    /// The cell space over the wire value (the demo: `Grid2` with the
    /// config's `aoi_cell_size` — the same knob the single-world AOI
    /// room turns).
    pub(in crate::sharded) space: S,
    /// The content bookkeeping shared with [`crate::aoi::AoiRoom`] —
    /// buckets over OWN entities AND borrowed records alike, change
    /// lists, member counts, born groups. Fed from two sources: the
    /// bevy dirty pass in `update` (own entities) and
    /// [`Self::integrate_borrowed`] (the strip diff).
    pub(in crate::sharded) book: CellBook<Wire<G>, S::Cell>,
    /// THE ledger (module docs, "THE borrowed-strip × delta-ledger
    /// subtlety"): the previous tick's flattened borrowed view,
    /// `wire id → wire value`. The new slice is diffed against THIS,
    /// never against the buckets, so an unchanged strip dirties nothing.
    pub(in crate::sharded) prev_borrowed: HashMap<u64, Wire<G>>,
    /// Once-per-tick guard for the strip integration + deferred roll:
    /// the tick whose broadcast-phase preparation has already run.
    pub(in crate::sharded) integrated_tick: u64,
    /// Per-player view baseline (`player → the cell whose FULL view was
    /// last delivered to it`): missing/other ⇒ the one-shot private
    /// full. Cleared on join/resume/migrate-in/migrate-out — a fresh
    /// session or a fresh shard MUST re-baseline (module docs, "Migration
    /// correctness").
    pub(in crate::sharded) conn_view: HashMap<PlayerId, S::Cell>,
    /// The global tick of the current step (set in `update`).
    pub(in crate::sharded) tick: u64,
    // ── Per-tick piece caches (cleared in `update`, computed lazily in
    //    the broadcast phase; order-independent across groups). ──
    pub(in crate::sharded) pieces: CellPieces<S::Cell>,
    /// The groups that emitted a FULL this tick (fresh group /
    /// keepalive): their members' private frames skip the one-shot.
    pub(in crate::sharded) group_full_emitted: HashSet<S::Cell>,
}

impl<G: ShardGame, P: Partition<Wire<G>>, S: CellSpace<Wire<G>>> ShardedSpatialRoom<G, P, S> {
    /// Build the spatial composite around the shard `inner`,
    /// broadcasting over the cell space `space` (see
    /// [`ShardedRoom::with_game`] for the shared halves).
    pub fn with_shard(inner: ShardedRoom<G, P>, space: S) -> Self {
        Self {
            inner,
            space,
            book: CellBook::default(),
            prev_borrowed: HashMap::new(),
            integrated_tick: 0,
            conn_view: HashMap::new(),
            tick: 0,
            pieces: CellPieces::default(),
            group_full_emitted: HashSet::new(),
        }
    }

    /// Opt the wrapped shard in to crystallization (see
    /// [`ShardedRoom::with_crystallize`]).
    #[must_use]
    pub fn with_crystallize(mut self, policy: Crystallize) -> Self {
        self.inner = self.inner.with_crystallize(policy);
        self
    }

    /// Set the disconnect-park grace on the wrapped shard (builder-style,
    /// like [`ShardedRoom::with_disconnect_grace`]; every shard of a room
    /// should carry the same policy).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.inner = self.inner.with_disconnect_grace(grace);
        self
    }

    /// Set the whole disconnect-park policy on the wrapped shard (see
    /// [`crate::room::OpenRoom::with_disconnect_policy`]; every shard of a
    /// room should carry the same policy).
    #[must_use]
    pub fn with_disconnect_policy(
        mut self,
        grace: Option<std::time::Duration>,
        to: gsb_core::room::ExpireTo,
    ) -> Self {
        self.inner = self.inner.with_disconnect_policy(grace, to);
        self
    }

    /// The game this shard runs.
    pub fn game(&self) -> &G {
        self.inner.game()
    }

    /// The game this shard runs, for configuration after construction.
    pub fn game_mut(&mut self) -> &mut G {
        self.inner.game_mut()
    }

    /// THE strip integration (module docs, "THE borrowed-strip ×
    /// delta-ledger subtlety") plus the deferred occupancy/birth roll:
    /// idempotent per tick, invoked at the top of every broadcast-phase
    /// hook. Diff the NEW borrowed slice against [`Self::prev_borrowed`]
    /// — entered/exited/moved only; an identical wire position records
    /// NOTHING (the evaporation guard) — feed the diff into the shared
    /// bookkeeping as non-member content, then roll the flags against
    /// the final bucket state.
    fn integrate_borrowed(&mut self, borrowed: &[BorderRecord<Wire<G>>]) {
        if self.integrated_tick == self.tick {
            return;
        }
        let mut new_view: HashMap<u64, Wire<G>> = HashMap::with_capacity(borrowed.len());
        for rec in borrowed {
            let value = &rec.state;
            // The frame filter, exactly as the plain shard applies it: a
            // neighbour exports its WHOLE border, and a record far from
            // this region is not this shard's content — even where a
            // cell's 3×3 would reach it. A filtered record is absent
            // from the view, so leaving the frame reads as an exit and
            // re-entering it as an entry.
            if !self.inner.partition.admits(self.inner.index, value) {
                continue;
            }
            new_view.insert(rec.wire, value.clone());
            match self.prev_borrowed.get(&rec.wire) {
                None => {
                    // Entered the visible set (first contact, a healing
                    // Full after quarantine, or a crossing-in): an upsert
                    // in its containing cell — borrowed content joins the
                    // cell's group content for members of that cell.
                    let c = self.space.cell_of(value);
                    self.book
                        .record_appearance(rec.wire, value.clone(), c, false);
                }
                Some(prev) if prev != value => {
                    // Moved: one upsert — or exit+upsert when the move
                    // crossed a cell boundary (the packet passes fix the
                    // wire order).
                    let old_c = self.space.cell_of(prev);
                    let new_c = self.space.cell_of(value);
                    if old_c == new_c {
                        self.book.record_update(new_c, rec.wire, value.clone());
                    } else {
                        self.book
                            .record_cross(old_c, new_c, rec.wire, value.clone(), false);
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
        // cells their previous records occupied — except where that cell
        // now holds this shard's OWN record of the same id (it migrated
        // in: the core's own-wins filter dropped the lent copy, and the
        // dirty pass placed the arrival in the very cell the lent copy
        // occupied, overwriting it). An exit there would erase the own
        // record (KIT-ARCHITECTURE §10, F1). A lent copy in ANOTHER cell
        // than the own record is stale and still exits.
        for (wire, prev) in &self.prev_borrowed {
            if !new_view.contains_key(wire) {
                let c = self.space.cell_of(prev);
                let own_here = self
                    .inner
                    .wire_entity
                    .get(wire)
                    .is_some_and(|e| self.book.cell_of_entity(e) == Some(c));
                if !own_here {
                    self.book.record_exit(c, *wire, false);
                }
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
    fn ensure_ready(&mut self, ctx: &TickCtx, borrowed: &[BorderRecord<Wire<G>>]) {
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
