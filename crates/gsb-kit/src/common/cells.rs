//! The cell-delta engine the AOI rooms share: the per-cell ledgers that
//! make one encoding serve every group that can see it. Generic over the
//! game's wire value `W` (the codec's `Wire`) and the space's cell key
//! `C` (KIT-ARCHITECTURE §4.1/§4.2); the kit keeps the envelopes, the
//! game supplies the record and cell-exit bodies.

mod book;
mod pieces;

pub(crate) use book::*;
pub(crate) use pieces::*;

/// The per-tick classification of one cell (a pure function of the
/// cell's change list and its occupancy baseline — identical for every
/// group that sees the cell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellFrag {
    /// Empty now, empty before, and no change recorded this tick:
    /// nothing for any group.
    Silent,
    /// Non-empty before, empty now: the group's packet carries one
    /// `CellExit` record for it (the client forgets the whole cell in
    /// one record).
    Exited,
    /// Empty before, non-empty now: no baseline exists for the cell —
    /// its FULL records go into the group's packet (upserts in a delta
    /// packet; full content in a fresh group's packet).
    Appeared,
    /// Non-empty before and now, content changed: the cell's delta
    /// piece (its change list: exits + updates).
    Delta,
}

/// One cell's changes this tick — the delta's source of truth: the
/// change list *is* the diff, so no per-cell content comparison is ever
/// run. Built incrementally from the dirty set / the borrowed diff;
/// persistent map, cleared in place each tick.
pub(crate) struct CellChanges<W> {
    /// Records whose wire content changed, or that newly occupy the cell
    /// (wire id, wire value). Encoded as `entities` upserts (field
    /// 2) — except for an appeared cell, whose group gets the cell's
    /// FULL piece instead (the upserts would be redundant: the client
    /// has no baseline for a cell that was empty).
    pub updates: Vec<(u64, W)>,
    /// Wire ids that left the cell (a cell-to-cell move or a despawn).
    /// Encoded as `removed` (field 3) — except when the whole cell
    /// exited, in which case one `CellExit` record supersedes them.
    pub exits: Vec<u64>,
    /// The cell was empty at the end of the last tick (set at roll
    /// time, order-independently — see [`CellBook::roll`]).
    pub appeared: bool,
    /// The cell is empty now (and was not empty then) — same evaluation.
    pub exited: bool,
}

// Not derived: a derive would demand `W: Default`, which a wire value
// need not be.
impl<W> Default for CellChanges<W> {
    fn default() -> Self {
        Self {
            updates: Vec::new(),
            exits: Vec::new(),
            appeared: false,
            exited: false,
        }
    }
}

/// The per-tick member-event counters of one touched cell: the
/// order-independent group-birth arithmetic reconstructs the before-tick
/// member count from `now − in + out`, so a same-tick exit+entry into
/// the same cell cannot fake a birth.
#[derive(Default)]
pub(crate) struct TouchInfo {
    /// Member entities that entered this cell this tick (joins into it,
    /// cell crossings into it).
    pub member_in: u32,
    /// Member entities that left it (crossings out, leavers).
    pub member_out: u32,
}
