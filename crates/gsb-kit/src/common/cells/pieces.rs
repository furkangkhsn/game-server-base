//! Encoded cell pieces: each cell is encoded ONCE per tick and shared
//! by reference with every group that can see it, then assembled into
//! one packet per group.

use std::collections::HashMap;
use std::hash::Hash;

use bytes::{Bytes, BytesMut};

use crate::codec::RecordCodec;
use crate::common::*;
use crate::space::CellSpace;

mod assemble;

pub(crate) use assemble::*;

/// The per-tick encoded-piece caches of a cell-delta broadcaster: every
/// piece is computed lazily ONCE per (cell, kind) per tick and shared as
/// frozen `Bytes` by reference with every group that needs it — the
/// "encode once, share the bytes" spine at cell granularity. Cleared in
/// place at each tick start.
///
/// Keyed by the space's cell `C`; the record and cell-exit bodies come
/// from the codec / the space each method is handed (the kit writes the
/// envelope around them).
pub(crate) struct CellPieces<C> {
    /// Each cell's encoded FULL records (the `entities` entries, field 2)
    /// of its current content.
    full_pieces: HashMap<C, Bytes>,
    /// Each changed cell's encoded delta: `(removed piece, entities piece)`
    /// assembled from the cell's change list.
    delta_pieces: HashMap<C, (Option<Bytes>, Bytes)>,
    /// Each exited cell's encoded `cell_exits` entry (field 4).
    exit_markers: HashMap<C, Bytes>,
    /// The assembled FULL snapshot of a cell's view (header + the full
    /// pieces): shared between the fresh-group packet, the keep-alive
    /// full, and the one-shot private full.
    full_view: HashMap<C, Bytes>,
    /// The scratch behind `full_view` (reused across assemblies;
    /// `split_to` hands out zero-copy views — no per-assembly allocation).
    scratch: BytesMut,
    /// The per-tick classification cache: the per-tick change list does
    /// not change during the tick, so the first caller's classification
    /// is valid for every later group and every later pass — including
    /// the negative answer (`Silent`), which is a hash miss on the change
    /// list, not a scan.
    frag_cache: HashMap<C, CellFrag>,
    /// Entity records encoded into pieces so far this tick (the overlap
    /// measurement: ~E per tick — one encoding per entity, in its own
    /// cell's piece). Read/reset via [`Self::take_encoded`].
    encoded: u64,
    /// The tick being broadcast (set by [`Self::begin_tick`]): the
    /// sequence stamped into the assembled fulls.
    tick: u64,
}

// Not derived: a derive would demand `C: Default`.
impl<C> Default for CellPieces<C> {
    fn default() -> Self {
        Self {
            full_pieces: HashMap::new(),
            delta_pieces: HashMap::new(),
            exit_markers: HashMap::new(),
            full_view: HashMap::new(),
            scratch: BytesMut::new(),
            frag_cache: HashMap::new(),
            encoded: 0,
            tick: 0,
        }
    }
}

impl<C: Copy + Eq + Hash> CellPieces<C> {
    /// Clear the per-tick caches (persistent containers, in place) and
    /// record the tick they are built for.
    pub(crate) fn begin_tick(&mut self, tick: u64) {
        self.full_pieces.clear();
        self.delta_pieces.clear();
        self.exit_markers.clear();
        self.full_view.clear();
        self.frag_cache.clear();
        self.encoded = 0;
        self.tick = tick;
    }

    /// Records encoded so far this tick (polled once per step via
    /// `GameLogic::encoded_records`).
    pub(crate) fn take_encoded(&mut self) -> u64 {
        std::mem::take(&mut self.encoded)
    }

    /// The per-tick classification of `c` (see [`CellFrag`]) — memoized
    /// per cell per tick, negative answer included.
    pub(crate) fn classify<W>(&mut self, changes: &HashMap<C, CellChanges<W>>, c: &C) -> CellFrag {
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
    pub(crate) fn full_piece<R: RecordCodec>(
        &mut self,
        codec: &R,
        buckets: &HashMap<C, HashMap<u64, R::Wire>>,
        c: &C,
    ) -> Option<Bytes> {
        if !self.full_pieces.contains_key(c)
            && let Some(bucket) = buckets.get(c)
        {
            self.encoded += bucket.len() as u64;
            let piece = encode_entity_records(codec, bucket.iter().map(|(&w, v)| (w, v)));
            self.full_pieces.insert(*c, piece);
        }
        self.full_pieces.get(c).cloned()
    }

    /// A changed cell's delta piece: `(exits, updates)` assembled from
    /// the cell's change list — the change list *is* the diff, so no
    /// per-cell content comparison is ever run (encoded once per tick).
    /// `None` when the cell has no change list (the caller classifies
    /// first; such a cell is silent for the tick).
    pub(crate) fn delta_piece<R: RecordCodec>(
        &mut self,
        codec: &R,
        changes: &HashMap<C, CellChanges<R::Wire>>,
        c: &C,
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
                    encode_entity_records(codec, ch.updates.iter().map(|(w, v)| (*w, v))),
                ),
            );
        }
        self.delta_pieces.get(c)
    }

    /// One `CellExit` marker for an exited cell (encoded once per tick).
    pub(crate) fn exit_marker<W, S: CellSpace<W, Cell = C>>(&mut self, space: &S, c: &C) -> Bytes {
        if !self.exit_markers.contains_key(c) {
            self.exit_markers
                .insert(*c, encode_cell_exit::<W, S>(space, *c));
        }
        self.exit_markers.get(c).expect("inserted above").clone()
    }

    /// The assembled FULL snapshot of `cell`'s view (header with
    /// `delta = false` + the full pieces of every non-empty cell of
    /// [`CellSpace::view`]) — computed once per tick and shared.
    pub(crate) fn full_view<R, S>(
        &mut self,
        codec: &R,
        space: &S,
        buckets: &HashMap<C, HashMap<u64, R::Wire>>,
        cell: &C,
    ) -> Bytes
    where
        R: RecordCodec,
        S: CellSpace<R::Wire, Cell = C>,
    {
        if let Some(bytes) = self.full_view.get(cell) {
            return bytes.clone();
        }
        self.scratch.clear();
        write_snapshot_header(&mut self.scratch, self.tick, false);
        for c in space.view(*cell) {
            if let Some(piece) = self.full_piece(codec, buckets, &c) {
                self.scratch.extend_from_slice(&piece);
            }
        }
        let bytes = self.scratch.split_to(self.scratch.len()).freeze();
        self.full_view.insert(*cell, bytes.clone());
        bytes
    }
}
