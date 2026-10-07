//! One group's packet from the book — `gsb-kit`'s
//! `assemble_group_packet` without the per-tick piece cache (the cache
//! is ECS-independent; the spike needs only the bytes to compare
//! bindings). The framing is a simplified `WorldSnapshot`: header,
//! `removed`, `cell_exits`, one record run.

use std::fmt::Debug;
use std::hash::Hash;

use crate::book::CellBook;
use crate::space::CellSpace;
use crate::wire::{WireValue, varint};

fn header(out: &mut Vec<u8>, tick: u64, delta: bool) {
    out.push(0x08);
    varint(tick, out);
    out.push(0x10);
    out.push(u8::from(delta));
}

fn record(out: &mut Vec<u8>, wire: u64, value: &impl WireValue) {
    varint(wire, out);
    value.encode(out);
}

/// A full piece: the cell's whole bucket, sorted by wire id (the
/// agnostic core's canonical order — `gsb-kit` iterates the bucket's
/// hash order).
fn full_piece<W: WireValue, C: Eq + Hash>(book: &CellBook<W, C>, c: &C, out: &mut Vec<u8>) {
    if let Some(bucket) = book.buckets.get(c) {
        let mut rows: Vec<(&u64, &W)> = bucket.iter().collect();
        rows.sort_unstable_by_key(|&(w, _)| *w);
        for (w, v) in rows {
            record(out, *w, v);
        }
    }
}

/// Assemble `cell`'s group packet for `tick`; `false` = silent.
pub fn assemble_group_packet<W, S>(
    book: &CellBook<W, S::Cell>,
    space: &S,
    cell: S::Cell,
    tick: u64,
    out: &mut Vec<u8>,
) -> bool
where
    W: WireValue,
    S: CellSpace<W>,
    S::Cell: Copy + Eq + Hash + Ord + Debug,
{
    if book.born_groups.contains(&cell) {
        header(out, tick, false);
        out.push(0x32);
        for c in space.view(cell) {
            full_piece(book, &c, out);
        }
        return true;
    }
    let ch = |c: &S::Cell| book.cell_changes.get(c);
    if !space.view(cell).any(|c| ch(&c).is_some()) {
        return false;
    }
    header(out, tick, true);
    for c in space.view(cell) {
        if let Some(x) = ch(&c)
            && !x.appeared
            && !x.exited
        {
            for w in &x.exits {
                out.push(0x18);
                varint(*w, out);
            }
        }
    }
    for c in space.view(cell) {
        if ch(&c).is_some_and(|x| x.exited) {
            out.push(0x22);
            space.encode_cell(c, out);
        }
    }
    out.push(0x32);
    for c in space.view(cell) {
        match ch(&c) {
            Some(x) if x.appeared => full_piece(book, &c, out),
            Some(x) if !x.exited => {
                for (w, v) in &x.updates {
                    record(out, *w, v);
                }
            }
            _ => {}
        }
    }
    true
}
