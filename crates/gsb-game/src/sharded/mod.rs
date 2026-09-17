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
//!
//! ## The spatial composite ([`ShardedSpatialRoom`] — ROADMAP Faz B)
//!
//! [`ShardedSpatialRoom`] is the `sharded × spatial` selection: the SAME
//! grid topology, migration protocol and border seam, but each shard's
//! broadcast phase groups its connections by **spatial cell**
//! (`GroupKey = Cell`, sized like [`crate::aoi::AoiRoom`]'s from the
//! config's `aoi_cell_size`) instead of one whole-shard group. The cell
//! encoding/delta engine itself is NOT duplicated: the shared
//! [`crate::common::CellBook`] / [`crate::common::CellPieces`] machinery
//! drives both rooms. What this module adds on top is exactly the part a
//! single world cannot have — the borrowed border strip — and it is the
//! load-bearing subtlety of the whole composite:
//!
//! ### THE borrowed-strip × delta-ledger subtlety (why a naive port decays)
//!
//! The borrowed records arrive at the broadcast phase FULLY REPLACED every
//! tick (the receiver-side view of the seq-stamped border-delta protocol
//! is kept wholesale by the core actor; quarantine aside, the flattened
//! slice always names every currently-borrowed entity and its CURRENT
//! truncated position). Diffing that slice against "the world as of last
//! tick" — the way own entities are diffed through bevy's change
//! detection — would therefore flag EVERY borrowed record as written on
//! EVERY tick: every cell touching the strip would go dirty tick after
//! tick, the deltas would degenerate toward full re-carries, and the
//! entire byte economy of the cell encoding would evaporate exactly where
//! shards touch (the densest places — players cluster near points of
//! interest, and POIs sit near seams by design).
//!
//! So the borrowed set participates in the delta bookkeeping through its
//! OWN ledger: the room keeps the PREVIOUS tick's borrowed view
//! (`prev_borrowed`: wire → wire position) and diffs the NEW view against
//! THAT — entered (absent before), exited (gone now), moved (position
//! changed), silent (identical wire position ⇒ NO change entry at all).
//! Only the diff lands in the shared [`crate::common::CellBook`] change
//! lists, so a static strip costs nothing beyond the comparison itself,
//! and a moving boundary entity produces exactly one upsert (plus an exit
//! when it changes cell) — the same shape an own-entity mover produces.
//!
//! Why ONE flat ledger instead of per-neighbor ledgers: the wire ids are
//! range-partitioned PER SHARD, so a borrowed id identifies exactly one
//! neighbor for the room's whole lifetime — the union of per-neighbor
//! diffs is mathematically the flat diff, minus a second level of maps.
//! (The core actor already merges the per-neighbor views into the sorted
//! slice this room receives; it also drops a neighbor's stale copy of an
//! entity that just migrated IN — own wins — which composes cleanly: the
//! ledger simply never saw that id, so no spurious exit is ever shipped
//! for it.)
//!
//! Two protocol events fall out of the ledger correctly BY CONSTRUCTION,
//! not by extra code: a QUARANTINED neighbor (a rejected border delta —
//! `stale_until_full`) vanishes from the flattened slice, which the
//! ledger renders as exits, and the healing Full re-renders as entries —
//! exactly what clients should see; and a crossing entity appears as an
//! exit on the leaving side's strip and an entry on the arriving side's,
//! with the own-wins filter swallowing the double view on the transition
//! tick (the accepted one-tick alignment blink, never a duplicate).
//!
//! ### Ordering: why the occupancy/birth roll cannot live in `update`
//!
//! Own entities are bucketed during `update` (the bevy dirty pass); the
//! borrowed diff can only run later — the core hands the strip to the
//! logic at the broadcast phase, after `update`. The shared engine's
//! appeared/exited/birth flags must reflect the FINAL content of the
//! tick, so the composite defers [`crate::common::CellBook::roll`] until
//! the first broadcast-phase call integrates the strip (a once-per-tick
//! guard; every snapshot/keepalive/private call is preceded by it). With
//! no connections there is no broadcast and no roll — and no client to
//! tell; the next broadcast's roll re-baselines from the final buckets
//! and any fresh group opens with a full packet regardless of flags.
//!
//! ### Migration correctness (fresh-member rule)
//!
//! A player migrating INTO a shard is dropped into a cell whose group may
//! be long-established — a delta would carry nothing to build their view
//! from. [`ShardLogic::on_migrate_in`] therefore clears the arrival's
//! view baseline, so the arrival's next `private` frame is the one-shot
//! FULL of their new 3×3 (skipped only when the group's own packet that
//! batch was already a full) — the same contract a late joiner gets on
//! the single-world AOI room. Symmetrically, a migrate-out removes the
//! leaver's baseline and parks the despawn removal (despawns are not
//! component writes) so the seam cell's delta carries the exit.

mod room;
mod spatial;

#[cfg(test)]
mod tests;

pub use room::ShardedRoom;
pub use spatial::ShardedSpatialRoom;

use gsb_core::id::PlayerId;

use crate::components::{MoveTarget, Position};

/// The demo's visibility-strip payload ([`GameLogic::Strip`]): the
/// entity's TRUNCATED position — exactly the content the core-fixed
/// boundary record carried before generalization, so this round changes
/// no wire bytes. A game needing more across the seam extends THIS type
/// (velocity, facing, hp snapshot); the core never learns about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripPos {
    pub x: i32,
    pub y: i32,
}

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
