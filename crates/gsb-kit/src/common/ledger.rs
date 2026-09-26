//! The SET-content delta ledger: the delta engine of a room whose group
//! content is a set of records — `wire id → wire value` — with no cell
//! structure a client could compute (team vision, a PVS sector union,
//! the open room's whole world).
//!
//! **Why not the cell engine.** [`CellBook`](super::CellBook) /
//! [`CellPieces`](super::CellPieces) encode a CELL once and share the
//! piece with every group whose view is a union of whole cells; the
//! group-independent per-cell delta is what makes that sound. A vision
//! set is not a union of cells — whether an enemy is in a team's set is
//! an exact distance test against that team's units, so one cell can be
//! half in a team's view — and a client cannot recompute membership from
//! a record (it would need the other team's positions). So the unit of
//! the delta here is the GROUP: its content is diffed against what its
//! clients hold, once per group per tick, and the frame is shared by the
//! group's members (the kit's "encode once per group" rule).
//!
//! **What the ledger holds (bounded).** Per group: the wire content of
//! the last frame emitted to it (`held` — the set its clients hold,
//! each record's wire value as its own fingerprint), the step it was
//! last asked for, and this step's full frame. Nothing grows with
//! history: `held` is bounded by the group's view.
//!
//! **The delta invariant.** A client that applied every frame the group
//! emitted holds exactly `held`: a delta is `content − held` (removals:
//! ids in `held` not in `content`; upserts: ids new to `held` or whose
//! wire value differs), after which `held = content`; a silent step
//! leaves both equal. Unlike the cell engine's "previous tick" baseline,
//! the diff is against the last EMITTED content, so a group that was
//! silent for any number of steps is still diffed correctly.
//! A client that lost frames is healed by the keep-alive full
//! ([`SetLedger::resync`]); a client without a baseline gets the one-shot
//! private full ([`Baselines`]).

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use bytes::{Bytes, BytesMut};

use crate::common::{Records, WriteRecord, put_removed, write_full_header, write_snapshot_header};

mod baselines;

pub(crate) use baselines::Baselines;

/// What a group's delta-mode emission wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Emitted {
    /// Nothing changed since the last emitted frame: no bytes.
    Silent,
    /// A FULL frame (a fresh group): every member is baselined by it.
    Full,
    /// A DELTA frame: `removed`, then the upserts.
    Delta,
}

/// One snapshot group's ledger (module docs).
pub(crate) struct SetLedger<W> {
    /// The wire content of the last frame emitted to the group.
    held: HashMap<u64, W>,
    /// The room step of the previous delta-mode emission call (`None`:
    /// never). The core asks every group with members once per step, so
    /// a gap means the group had no members in between: it is fresh.
    seen: Option<u64>,
    /// This step's FULL frame, encoded at most once per step and shared
    /// by the fresh-group frame, the keep-alive full and every one-shot
    /// private full.
    full: Option<(u64, Bytes)>,
    /// The step whose group frame was a full (fresh or keep-alive).
    full_sent: Option<u64>,
}

// Not derived: a derive would demand `W: Default`.
impl<W> Default for SetLedger<W> {
    fn default() -> Self {
        Self {
            held: HashMap::new(),
            seen: None,
            full: None,
            full_sent: None,
        }
    }
}

impl<W: Clone + Eq> SetLedger<W> {
    /// FULL mode — the full-only rooms' frame, byte for byte: nothing
    /// when `content` equals the last emitted content (`false`), else
    /// the whole content (records in `content`'s iteration order) under
    /// the full header. `encoded` counts the records written.
    pub(crate) fn emit_full<R: WriteRecord<W>>(
        &mut self,
        codec: &R,
        tick: u64,
        content: &HashMap<u64, W>,
        out: &mut BytesMut,
        encoded: &mut u64,
    ) -> bool {
        if self.held == *content {
            return false;
        }
        write_full_header(out, tick);
        let records = Records::open(R::RUN, out);
        for (id, wire) in content {
            codec.put(*id, wire, out);
        }
        records.close(out);
        *encoded += content.len() as u64;
        self.held = content.clone();
        true
    }

    /// DELTA mode: a fresh group (not asked for on the previous `step`)
    /// gets the FULL frame; an established one the DELTA against what
    /// its clients hold — `removed` (ids that left) first, then the
    /// upserts (new ids, and changed wire values DUE on `step` — the
    /// record's send rate, A10: a changed record that is not due keeps
    /// its held value and goes out, current, on a later due step) in
    /// the codec's record framing — or nothing when nothing is to be
    /// sent. `encoded` counts the records written.
    pub(crate) fn emit_delta<R: WriteRecord<W>>(
        &mut self,
        codec: &R,
        step: u64,
        tick: u64,
        content: &HashMap<u64, W>,
        out: &mut BytesMut,
        encoded: &mut u64,
    ) -> Emitted {
        let fresh = self.seen.is_none_or(|seen| seen.wrapping_add(1) != step);
        self.seen = Some(step);
        if fresh {
            let full = self.resync(codec, step, tick, content, encoded);
            out.extend_from_slice(&full);
            return Emitted::Full;
        }
        let start = out.len();
        write_snapshot_header(out, tick, true);
        let body = out.len();
        self.held.retain(|id, _| {
            let stays = content.contains_key(id);
            if !stays {
                put_removed(out, *id);
            }
            stays
        });
        let records = Records::open(R::RUN, out);
        for (id, wire) in content {
            match self.held.entry(*id) {
                Entry::Occupied(held) if held.get() == wire => continue,
                // Changed, but not due (A10): the clients keep what they
                // hold — and so does the ledger, so the change is still a
                // difference on the record's due step.
                Entry::Occupied(held) if !codec.due(step, *id, held.get(), wire) => continue,
                Entry::Occupied(mut held) => {
                    held.insert(wire.clone());
                }
                Entry::Vacant(slot) => {
                    slot.insert(wire.clone());
                }
            }
            codec.put(*id, wire, out);
            *encoded += 1;
        }
        records.close(out);
        if out.len() == body {
            out.truncate(start);
            return Emitted::Silent;
        }
        Emitted::Delta
    }

    /// The group's FULL frame for this step (the complete `content`
    /// under the full header — the same bytes the full mode writes for
    /// it), encoded once per step and shared.
    pub(crate) fn full_frame<R: WriteRecord<W>>(
        &mut self,
        codec: &R,
        step: u64,
        tick: u64,
        content: &HashMap<u64, W>,
        encoded: &mut u64,
    ) -> Bytes {
        if let Some((at, bytes)) = &self.full
            && *at == step
        {
            return bytes.clone();
        }
        let mut buf = BytesMut::new();
        write_full_header(&mut buf, tick);
        let records = Records::open(R::RUN, &mut buf);
        for (id, wire) in content {
            codec.put(*id, wire, &mut buf);
        }
        records.close(&mut buf);
        *encoded += content.len() as u64;
        let bytes = buf.freeze();
        self.full = Some((step, bytes.clone()));
        bytes
    }

    /// The group's frame is a FULL this step (a fresh group, a keep-alive
    /// tick): [`Self::full_frame`], with the ledger re-synced to it.
    pub(crate) fn resync<R: WriteRecord<W>>(
        &mut self,
        codec: &R,
        step: u64,
        tick: u64,
        content: &HashMap<u64, W>,
        encoded: &mut u64,
    ) -> Bytes {
        let full = self.full_frame(codec, step, tick, content, encoded);
        self.held.clone_from(content);
        self.full_sent = Some(step);
        full
    }

    /// Whether the group's own frame on `step` was a full — it precedes
    /// the private frames in every member's batch, so it baselined them.
    pub(crate) fn full_sent(&self, step: u64) -> bool {
        self.full_sent == Some(step)
    }
}

#[cfg(test)]
mod tests;
