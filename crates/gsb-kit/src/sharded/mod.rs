//! [`ShardedRoom`]: the shard-level [`ShardLogic`], generic over the game
//! (`G: ShardGame`) and the map partition (`P: Partition<Wire<G>>`,
//! KIT-ARCHITECTURE §4.2). The sections below speak in the demo's terms
//! (the kit's `GridPartition2` preset over the demo's `Position`).
//!
//! ## What it is
//!
//! The same game as the single-world rooms, but the world is
//! partitioned: a room of `shard_count` actors, each owning one region
//! of the map. The core machinery (`gsb_core::shard`) runs the shard
//! protocol (migration, border exchange, range-partitioned wire ids);
//! this module supplies what the core leaves to the logic:
//!
//! - the **region** of a position and the **neighbor** topology (the
//!   partition — the demo: a grid, 4-neighbourhood), plus the migration
//!   routing over it (a region that is not a neighbour's is reached hop
//!   by hop, §8.4),
//! - the **migration state** ([`KitMig`]: the game's captured state —
//!   `ShardGame::capture`/`restore` — plus the kit's park record),
//! - the **border** export/import (boundary visibility; the demo grid:
//!   a quarter-cell margin on each side), the strip payload being the
//!   codec's wire value (`Strip = Wire`),
//! - the **snapshot** (the shard's own world + the borrowed boundary
//!   records, one group — `GroupKey = ()`).
//!
//! ## Visibility model (what a player sees)
//!
//! A player sees **its shard's whole region plus a boundary margin**: the
//! own region (a `1/shard_count` slice of the map, in a grid cell) and the
//! neighboring shards' boundary entities within the border margin of the
//! shared edge. This is distance-limited visibility — the class of game sharding
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
//! The view is the FILTERED strip: every record first passes the frame
//! filter the plain shard applies ([`crate::space::Partition::admits`] —
//! a neighbour exports its whole border, and the parts far from this
//! region are not this shard's content, even where a cell's 3×3 would
//! reach them), so leaving the frame reads as an exit and entering it
//! as an entry.
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
//! entity that just migrated IN — own wins. The ledger HAS seen that id —
//! a crossing entity is near the seam, so it was lent in until the tick it
//! arrived — and reads the drop as an exit. Where the lent copy sat in
//! the cell the arrival's own record now occupies, that exit is skipped:
//! it would erase the own record from its bucket (KIT-ARCHITECTURE §10,
//! F1 — the arrival missing from its own one-shot full, an entity alone
//! in its cell invisible on its new shard until it moved cells). A lent
//! copy in another cell is stale and exits as usual.)
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

mod crystal;
mod departing;
mod mig;
mod room;
mod seam;
mod spatial;

#[cfg(test)]
mod tests;

pub use crystal::Crystallize;
pub use mig::{KitMig, ShardInputRecord, ShardParkRecord, ShardPin};
pub use room::ShardedRoom;
pub use seam::Seam;
pub use spatial::ShardedSpatialRoom;

// The grid helpers, in scope for the in-module tests (`use super::*`).
#[cfg(test)]
use crate::space::{grid_shape, shard_at};

/// The migration routing table of shard `from` over a partition of
/// `shard_count` regions whose neighbour lists `neighbors_of` returns:
/// for every region, the neighbour of `from` that begins a shortest path
/// to it (breadth-first, neighbours in their listed order — deterministic).
/// `from` maps to itself; a region the graph cannot reach maps to
/// `usize::MAX` (never a neighbour: such an entity is never handed on).
///
/// The core hands a migrating entity only to a NEIGHBOUR
/// (`ShardLogic::collect_migrations(neighbor)`, called for each of
/// `neighbors()`), so an entity whose region is further away travels
/// hop by hop: each intermediate shard installs it and routes it on.
pub(in crate::sharded) fn first_hops(
    from: usize,
    shard_count: usize,
    neighbors_of: impl Fn(usize) -> Vec<usize>,
) -> Vec<usize> {
    let mut hop = vec![usize::MAX; shard_count];
    hop[from] = from;
    let mut queue = std::collections::VecDeque::new();
    for n in neighbors_of(from) {
        if hop[n] == usize::MAX {
            hop[n] = n;
            queue.push_back(n);
        }
    }
    while let Some(at) = queue.pop_front() {
        for n in neighbors_of(at) {
            if hop[n] == usize::MAX {
                hop[n] = hop[at];
                queue.push_back(n);
            }
        }
    }
    hop
}
