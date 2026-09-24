//! Encoded cell pieces: each cell is encoded ONCE per tick and shared
//! by reference with every group that can see it, then assembled into
//! one packet per group.

use std::collections::{HashMap, HashSet};

use bytes::{Bytes, BytesMut};

use crate::kit::common::*;

/// The per-tick encoded-piece caches of a cell-delta broadcaster: every
/// piece is computed lazily ONCE per (cell, kind) per tick and shared as
/// frozen `Bytes` by reference with every group that needs it — the
/// "encode once, share the bytes" spine at cell granularity. Cleared in
/// place at each tick start.
#[derive(Default)]
pub(crate) struct CellPieces {
    /// Each cell's encoded FULL records (the `entities` entries, field 2)
    /// of its current content.
    full_pieces: HashMap<Cell, Bytes>,
    /// Each changed cell's encoded delta: `(removed piece, entities piece)`
    /// assembled from the cell's change list.
    delta_pieces: HashMap<Cell, (Option<Bytes>, Bytes)>,
    /// Each exited cell's encoded `cell_exits` entry (field 4).
    exit_markers: HashMap<Cell, Bytes>,
    /// The assembled FULL snapshot of a cell's 3×3 view (header + the
    /// full pieces): shared between the fresh-group packet, the keep-
    /// alive full, and the one-shot private full.
    full_view: HashMap<Cell, Bytes>,
    /// The scratch behind `full_view` (reused across assemblies;
    /// `split_to` hands out zero-copy views — no per-assembly allocation).
    scratch: BytesMut,
    /// The per-tick classification cache: the per-tick change list does
    /// not change during the tick, so the first caller's classification
    /// is valid for every later group and every later pass — including
    /// the negative answer (`Silent`), which is a hash miss on the change
    /// list, not a scan.
    frag_cache: HashMap<Cell, CellFrag>,
    /// Entity records encoded into pieces so far this tick (the overlap
    /// measurement: ~E per tick — one encoding per entity, in its own
    /// cell's piece). Read/reset via [`Self::take_encoded`].
    encoded: u64,
}

impl CellPieces {
    /// Clear the per-tick caches (persistent containers, in place).
    pub(crate) fn begin_tick(&mut self) {
        self.full_pieces.clear();
        self.delta_pieces.clear();
        self.exit_markers.clear();
        self.full_view.clear();
        self.frag_cache.clear();
        self.encoded = 0;
    }

    /// Records encoded so far this tick (polled once per step via
    /// `GameLogic::encoded_records`).
    pub(crate) fn take_encoded(&mut self) -> u64 {
        std::mem::take(&mut self.encoded)
    }

    /// The per-tick classification of `c` (see [`CellFrag`]) — memoized
    /// per cell per tick, negative answer included.
    pub(crate) fn classify(&mut self, changes: &HashMap<Cell, CellChanges>, c: &Cell) -> CellFrag {
        if let Some(&frag) = self.frag_cache.get(c) {
            return frag;
        }
        let frag = match changes.get(c) {
            None => CellFrag::Silent,
            Some(ch) if ch.appeared => CellFrag::Appeared,
            Some(ch) if ch.exited => CellFrag::Exited,
            Some(_) => CellFrag::Delta,
        };
        self.frag_cache.insert(*c, frag);
        frag
    }

    /// The cell's FULL piece (its complete current content, encoded once
    /// per tick; `None` for an empty cell).
    pub(crate) fn full_piece(
        &mut self,
        buckets: &HashMap<Cell, HashMap<u64, (i32, i32)>>,
        c: &Cell,
    ) -> Option<Bytes> {
        if !self.full_pieces.contains_key(c)
            && let Some(bucket) = buckets.get(c)
        {
            let records: Vec<(u64, i32, i32)> =
                bucket.iter().map(|(&w, &(x, y))| (w, x, y)).collect();
            self.encoded += records.len() as u64;
            self.full_pieces.insert(*c, encode_entity_records(&records));
        }
        self.full_pieces.get(c).cloned()
    }

    /// A changed cell's delta piece: `(exits, updates)` assembled from
    /// the cell's change list — the change list *is* the diff, so no
    /// per-cell content comparison is ever run (encoded once per tick).
    /// `None` when the cell has no change list (the caller classifies
    /// first; such a cell is silent for the tick).
    pub(crate) fn delta_piece(
        &mut self,
        changes: &HashMap<Cell, CellChanges>,
        c: &Cell,
    ) -> Option<&(Option<Bytes>, Bytes)> {
        if !self.delta_pieces.contains_key(c)
            && let Some(ch) = changes.get(c)
            && (!ch.exits.is_empty() || !ch.updates.is_empty())
        {
            self.encoded += ch.updates.len() as u64;
            self.delta_pieces.insert(
                *c,
                (
                    (!ch.exits.is_empty()).then(|| encode_entity_exits(&ch.exits)),
                    encode_entity_records(&ch.updates),
                ),
            );
        }
        self.delta_pieces.get(c)
    }

    /// One `CellExit` marker for an exited cell (encoded once per tick).
    pub(crate) fn exit_marker(&mut self, c: &Cell) -> Bytes {
        if !self.exit_markers.contains_key(c) {
            self.exit_markers.insert(*c, encode_cell_exit(*c));
        }
        self.exit_markers.get(c).expect("inserted above").clone()
    }

    /// The assembled FULL snapshot of `cell`'s 3×3 view (header with
    /// `delta = false` + the full pieces of every non-empty cell) —
    /// computed once per tick and shared.
    pub(crate) fn full_view(
        &mut self,
        buckets: &HashMap<Cell, HashMap<u64, (i32, i32)>>,
        tick: u64,
        cell: &Cell,
    ) -> Bytes {
        if let Some(bytes) = self.full_view.get(cell) {
            return bytes.clone();
        }
        self.scratch.clear();
        write_snapshot_header(&mut self.scratch, tick, false);
        for (dx, dy) in BLOCK_OFFSETS {
            let c = Cell(cell.0 + dx, cell.1 + dy);
            if let Some(piece) = self.full_piece(buckets, &c) {
                self.scratch.extend_from_slice(&piece);
            }
        }
        let bytes = self.scratch.split_to(self.scratch.len()).freeze();
        self.full_view.insert(*cell, bytes.clone());
        bytes
    }
}

/// Assemble one group's packet from this tick's pieces: a FRESH group
/// (it had no members at the last roll) gets a FULL packet of its 3×3
/// view; an ESTABLISHED group gets a DELTA packet — exits (field 3),
/// then cell exits (field 4), then updates/appeared fulls (field 2) —
/// or nothing (`false`) when the whole block is silent. Marks a fresh
/// group's full in `group_full_emitted` (the batch-ordering signal the
/// private frame uses to skip its one-shot).
pub(crate) fn assemble_group_packet(
    pieces: &mut CellPieces,
    book: &CellBook,
    cell: &Cell,
    group_full_emitted: &mut HashSet<Cell>,
    tick: u64,
    out: &mut BytesMut,
) -> bool {
    if book.born_groups.contains(cell) {
        // Fresh group: every member is new to this view — the first
        // packet is a full (delta=false), so the members end the tick
        // baselined (the invariant starts from here).
        group_full_emitted.insert(*cell);
        let full = pieces.full_view(&book.buckets, tick, cell);
        out.extend_from_slice(&full);
        return true;
    }
    // Established group: emit a delta only when at least one cell in the
    // block has something to say (silence writes no bytes).
    let mut any = false;
    for (dx, dy) in BLOCK_OFFSETS {
        let c = Cell(cell.0 + dx, cell.1 + dy);
        if pieces.classify(&book.cell_changes, &c) != CellFrag::Silent {
            any = true;
        }
    }
    if !any {
        return false;
    }
    write_snapshot_header(out, tick, true);
    // Pass 1: entity exits of every cell — exits before updates, so a
    // cell-to-cell move is exited from its source before it is updated
    // in its target.
    for (dx, dy) in BLOCK_OFFSETS {
        let c = Cell(cell.0 + dx, cell.1 + dy);
        if pieces.classify(&book.cell_changes, &c) == CellFrag::Delta
            && let Some((exits, _)) = pieces.delta_piece(&book.cell_changes, &c)
            && let Some(e) = exits
        {
            out.extend_from_slice(e);
        }
    }
    // Pass 2: cell exits — one record per cell that became empty.
    for (dx, dy) in BLOCK_OFFSETS {
        let c = Cell(cell.0 + dx, cell.1 + dy);
        if pieces.classify(&book.cell_changes, &c) == CellFrag::Exited {
            out.extend_from_slice(&pieces.exit_marker(&c));
        }
    }
    // Pass 3: updates — delta pieces' changed records, and appeared
    // cells' full records (upserts — no baseline exists for a cell that
    // was empty).
    for (dx, dy) in BLOCK_OFFSETS {
        let c = Cell(cell.0 + dx, cell.1 + dy);
        match pieces.classify(&book.cell_changes, &c) {
            CellFrag::Delta => {
                if let Some((_, updates)) = pieces.delta_piece(&book.cell_changes, &c) {
                    out.extend_from_slice(updates);
                }
            }
            CellFrag::Appeared => {
                if let Some(piece) = pieces.full_piece(&book.buckets, &c) {
                    out.extend_from_slice(&piece);
                }
            }
            _ => {}
        }
    }
    true
}
