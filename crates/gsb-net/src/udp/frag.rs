//! Game-band fragmentation: the FRAG datagram kind, the server-side
//! split and the client-side reassembly (see the module docs of
//! `crate::udp`, "MTU (feature 3)", for the policy and its bounds).
//!
//! A fragmented message is the byte string a RAW datagram would carry
//! after its kind byte — `[u16 LE op][payload]` — cut into equal chunks
//! (the last one shorter):
//!
//! ```text
//! 4 FRAG [u16 LE message id][u8 index][u8 count][chunk]
//! ```
//!
//! Only a game-band frame over the datagram budget is fragmented; every
//! other datagram keeps its exact bytes. Loss semantics are the RAW
//! band's: a message is delivered only when all of its fragments have
//! arrived, and a message with a missing fragment is dropped (never
//! retransmitted) once it is superseded or too old — the next full
//! snapshot heals the view.

use std::time::{Duration, Instant};

use gsb_protocol::FrameBody;

use crate::udp::*;

/// The FRAG header: kind + message id + index + count.
pub(super) const FRAG_HEADER: usize = 5;
/// The most fragments one message may have. With the default budget a
/// message may be up to 16 × 1467 = 23 472 bytes; a larger one takes the
/// drop+count path instead (see the module docs for the derivation).
pub(super) const FRAG_MAX_COUNT: usize = 16;
/// Concurrent messages under reassembly per session (a power of two:
/// the slot of message `id` is `id % FRAG_SLOTS`).
pub(super) const FRAG_SLOTS: usize = 4;
/// Per-session reassembly memory: the chunk bytes held across all slots.
pub(super) const FRAG_MEM_CAP: usize = 64 * 1024;
/// A message whose fragments have not all arrived this long after its
/// first one is dropped (the next fragment to arrive sweeps it).
pub(super) const FRAG_MAX_AGE: Duration = Duration::from_millis(250);

/// Cut `body` (a RAW datagram's bytes after its kind byte) into the
/// FRAG datagrams of message `id`: equal chunks of `max_datagram −`
/// [`FRAG_HEADER`] bytes, the last one shorter. `None` when the message
/// cannot be sent this way — over [`FRAG_MAX_COUNT`] fragments, or a
/// budget too small to carry a chunk.
pub(super) fn split(id: u16, body: &[u8], max_datagram: usize) -> Option<Vec<Vec<u8>>> {
    let chunk = max_datagram.checked_sub(FRAG_HEADER).filter(|&c| c > 0)?;
    let count = body.len().div_ceil(chunk);
    if count > FRAG_MAX_COUNT {
        return None;
    }
    // `count <= FRAG_MAX_COUNT` (16): the index and the count fit a u8.
    Some(
        body.chunks(chunk)
            .enumerate()
            .map(|(index, part)| encode_frag(id, index as u8, count as u8, part))
            .collect(),
    )
}

/// Encode one FRAG datagram.
fn encode_frag(id: u16, index: u8, count: u8, chunk: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(FRAG_HEADER + chunk.len());
    d.push(KIND_FRAG);
    d.extend_from_slice(&id.to_le_bytes());
    d.push(index);
    d.push(count);
    d.extend_from_slice(chunk);
    d
}

/// `a` is a later message id than `b` (serial-number order, so the id
/// wraps without ever comparing "newer" across the wrap the wrong way).
fn newer(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}

/// One reassembly slot.
#[derive(Debug, Default)]
enum Slot {
    #[default]
    Empty,
    /// A message with at least one fragment and at least one missing.
    Partial {
        id: u16,
        count: u8,
        /// One entry per index; empty = not yet arrived (a chunk is
        /// never empty).
        parts: Vec<Vec<u8>>,
        have: u8,
        started: Instant,
    },
    /// A delivered message: its late duplicates are ignored until the
    /// slot is reused or ages out.
    Done { id: u16, at: Instant },
}

/// The client's reassembly state: [`FRAG_SLOTS`] fixed slots, a byte
/// counter, nothing else — no map, no growth.
#[derive(Debug, Default)]
pub(super) struct Reassembly {
    slots: [Slot; FRAG_SLOTS],
    /// Chunk bytes currently held by `Partial` slots.
    buffered: usize,
}

impl Reassembly {
    /// Handle one FRAG datagram (`d` includes the kind byte). Returns the
    /// message when this fragment completed it.
    pub(super) fn accept(
        &mut self,
        d: &[u8],
        now: Instant,
        stats: &mut UdpClientStats,
    ) -> Option<FrameBody> {
        if d.len() <= FRAG_HEADER {
            stats.frag_rejected += 1;
            return None;
        }
        let id = u16::from_le_bytes([d[1], d[2]]);
        let (index, count) = (d[3], d[4]);
        let chunk = &d[FRAG_HEADER..];
        if count < 2 || usize::from(count) > FRAG_MAX_COUNT || index >= count {
            stats.frag_rejected += 1;
            return None;
        }
        self.sweep(now, stats);
        let at = usize::from(id) % FRAG_SLOTS;
        match &self.slots[at] {
            Slot::Done { id: done, .. } if *done == id => return None, // late duplicate
            Slot::Partial { id: p, .. } | Slot::Done { id: p, .. } if *p != id => {
                if newer(*p, id) {
                    // A fragment of a message this slot has already
                    // moved past: superseded, never resurrected.
                    stats.frag_rejected += 1;
                    return None;
                }
                self.evict(at, stats);
            }
            _ => {}
        }
        if matches!(self.slots[at], Slot::Empty) {
            self.slots[at] = Slot::Partial {
                id,
                count,
                parts: vec![Vec::new(); usize::from(count)],
                have: 0,
                started: now,
            };
        }
        let Slot::Partial {
            count: c, parts, ..
        } = &self.slots[at]
        else {
            return None;
        };
        if *c != count {
            // The same message cannot change its fragment count.
            stats.frag_rejected += 1;
            return None;
        }
        if !parts[usize::from(index)].is_empty() {
            return None; // duplicate fragment
        }
        if !self.make_room(at, chunk.len(), stats) {
            self.evict(at, stats);
            return None;
        }
        let Slot::Partial { parts, have, .. } = &mut self.slots[at] else {
            return None;
        };
        parts[usize::from(index)].extend_from_slice(chunk);
        *have += 1;
        let complete = *have == count;
        self.buffered += chunk.len();
        if !complete {
            return None;
        }
        let Slot::Partial { parts, .. } = std::mem::take(&mut self.slots[at]) else {
            return None;
        };
        let body: Vec<u8> = parts.concat();
        self.buffered -= body.len();
        self.slots[at] = Slot::Done { id, at: now };
        stats.frag_reassembled += 1;
        body_of(&body, 0)
    }

    /// Age out: a partial older than [`FRAG_MAX_AGE`] is dropped (and
    /// counted); a done marker that old is simply forgotten.
    fn sweep(&mut self, now: Instant, stats: &mut UdpClientStats) {
        for at in 0..FRAG_SLOTS {
            let old = match &self.slots[at] {
                Slot::Partial { started, .. } => now.duration_since(*started) > FRAG_MAX_AGE,
                Slot::Done { at: t, .. } => now.duration_since(*t) > FRAG_MAX_AGE,
                Slot::Empty => false,
            };
            if old {
                self.evict(at, stats);
            }
        }
    }

    /// Free slot `at`; a partial message in it is counted as dropped.
    fn evict(&mut self, at: usize, stats: &mut UdpClientStats) {
        if let Slot::Partial { parts, .. } = std::mem::take(&mut self.slots[at]) {
            self.buffered -= parts.iter().map(Vec::len).sum::<usize>();
            stats.frag_dropped_incomplete += 1;
        }
    }

    /// Keep [`FRAG_MEM_CAP`]: evict OTHER partial messages, oldest first,
    /// until `extra` more bytes fit. `false` when even that is not enough.
    fn make_room(&mut self, keep: usize, extra: usize, stats: &mut UdpClientStats) -> bool {
        while self.buffered + extra > FRAG_MEM_CAP {
            let oldest = (0..FRAG_SLOTS)
                .filter(|&i| i != keep)
                .filter_map(|i| match &self.slots[i] {
                    Slot::Partial { started, .. } => Some((*started, i)),
                    _ => None,
                })
                .min();
            match oldest {
                Some((_, i)) => self.evict(i, stats),
                None => return false,
            }
        }
        true
    }

    /// Chunk bytes currently held (the memory bound's measure).
    #[cfg(test)]
    pub(super) fn buffered(&self) -> usize {
        self.buffered
    }
}

#[cfg(test)]
mod tests;
