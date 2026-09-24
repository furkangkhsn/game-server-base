//! Packet assembly: one group's packet, concatenated from this tick's
//! shared cell pieces in the fixed wire order (a child of `pieces`: it
//! reads the caches' tick).

use std::collections::HashSet;

use bytes::BytesMut;

use crate::kit::codec::RecordCodec;
use crate::kit::common::*;
use crate::kit::space::CellSpace;

/// Assemble one group's packet from this tick's pieces: a FRESH group
/// (it had no members at the last roll) gets a FULL packet of its view; an ESTABLISHED group gets a DELTA packet — exits (field 3),
/// then cell exits (field 4), then updates/appeared fulls (field 2) —
/// or nothing (`false`) when the whole block is silent. Marks a fresh
/// group's full in `group_full_emitted` (the batch-ordering signal the
/// private frame uses to skip its one-shot).
pub(crate) fn assemble_group_packet<R, S>(
    pieces: &mut CellPieces<S::Cell>,
    book: &CellBook<R::Wire, S::Cell>,
    codec: &R,
    space: &S,
    cell: &S::Cell,
    group_full_emitted: &mut HashSet<S::Cell>,
    out: &mut BytesMut,
) -> bool
where
    R: RecordCodec,
    S: CellSpace<R::Wire>,
{
    if book.born_groups.contains(cell) {
        // Fresh group: every member is new to this view — the first
        // packet is a full (delta=false), so the members end the tick
        // baselined (the invariant starts from here).
        group_full_emitted.insert(*cell);
        let full = pieces.full_view(codec, space, &book.buckets, cell);
        out.extend_from_slice(&full);
        return true;
    }
    // Established group: emit a delta only when at least one cell in the
    // block has something to say (silence writes no bytes).
    let mut any = false;
    for c in space.view(*cell) {
        if pieces.classify(&book.cell_changes, &c) != CellFrag::Silent {
            any = true;
        }
    }
    if !any {
        return false;
    }
    write_snapshot_header(out, pieces.tick, true);
    // Pass 1: entity exits of every cell — exits before updates, so a
    // cell-to-cell move is exited from its source before it is updated
    // in its target.
    for c in space.view(*cell) {
        if pieces.classify(&book.cell_changes, &c) == CellFrag::Delta
            && let Some((exits, _)) = pieces.delta_piece(codec, &book.cell_changes, &c)
            && let Some(e) = exits
        {
            out.extend_from_slice(e);
        }
    }
    // Pass 2: cell exits — one record per cell that became empty.
    for c in space.view(*cell) {
        if pieces.classify(&book.cell_changes, &c) == CellFrag::Exited {
            out.extend_from_slice(&pieces.exit_marker(space, &c));
        }
    }
    // Pass 3: updates — delta pieces' changed records, and appeared
    // cells' full records (upserts — no baseline exists for a cell that
    // was empty).
    for c in space.view(*cell) {
        match pieces.classify(&book.cell_changes, &c) {
            CellFrag::Delta => {
                if let Some((_, updates)) = pieces.delta_piece(codec, &book.cell_changes, &c) {
                    out.extend_from_slice(updates);
                }
            }
            CellFrag::Appeared => {
                if let Some(piece) = pieces.full_piece(codec, &book.buckets, &c) {
                    out.extend_from_slice(&piece);
                }
            }
            _ => {}
        }
    }
    true
}
