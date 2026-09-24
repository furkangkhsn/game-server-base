//! The client's local view of the world: applying snapshots, fulls
//! and deltas exactly as a real client would, so a wrong stream is
//! visible as a wrong view.

use super::*;
use std::collections::HashMap;

mod run;
pub(crate) use run::*;

/// The client-side world view the protocol prescribes (see the
/// `WorldSnapshot` docs in `gsb_demo/proto/game.proto`): a full REPLACES
/// the view; a delta applies ON TOP in the fixed order `removed` →
/// `cell_exits` → `entities` — even across a sequence gap (the stream is
/// event-driven: a gap is normal, and the records are absolute, so a
/// stale view is the worst case; the keep-alive full is the convergence
/// guarantee); a delta with NO baseline at all (a fresh client) is
/// DROPPED until the next full; a duplicate/stale sequence (<= the last
/// accepted) is discarded. The cell of a stored position uses the server's own formula
/// (floor of the WIRE coordinates / cell_size), so a `CellExit` record
/// forgets exactly the entities the server considers to be in that cell.
pub(crate) struct ClientView {
    pub(crate) entities: HashMap<u64, (i32, i32)>,
    pub(crate) last_seq: Option<u64>,
    pub(crate) cell_size: f32,
}

/// The outcome of applying one snapshot (the report's counters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Apply {
    /// A full was applied (the view was replaced).
    Full,
    /// A delta was applied on top (consecutive or across a gap — the
    /// stream is event-driven, so a gap is normal, not loss; the
    /// keep-alive full is the convergence guarantee).
    Delta,
    /// A delta dropped with no baseline at all (a fresh client before
    /// its first full — healed by the one-shot private full / the next
    /// keep-alive full).
    NoBaseline,
    /// A duplicate/stale sequence: discarded (not an error).
    Stale,
}

impl ClientView {
    #[inline]
    fn cell_of(x: i32, y: i32, cell_size: f32) -> (i32, i32) {
        (
            (x as f32 / cell_size).floor() as i32,
            (y as f32 / cell_size).floor() as i32,
        )
    }

    fn apply(&mut self, s: &gsb_demo::game::WorldSnapshot) -> Apply {
        if s.sequence <= self.last_seq.unwrap_or(0) {
            return Apply::Stale;
        }
        if s.delta {
            // A delta needs a baseline (a full applied before it); a
            // sequence gap does NOT disqualify it (event-driven stream —
            // see the message docs in `game.proto`).
            if self.last_seq.is_none() {
                return Apply::NoBaseline;
            }
            for &w in &s.removed {
                self.entities.remove(&w);
            }
            for c in &s.cell_exits {
                let cell = Self::cell_of(c.x, c.y, self.cell_size);
                self.entities
                    .retain(|_, (x, y)| Self::cell_of(*x, *y, self.cell_size) != cell);
            }
            for e in &s.entities {
                self.entities.insert(e.entity, (e.x, e.y));
            }
            self.last_seq = Some(s.sequence);
            return Apply::Delta;
        }
        self.entities.clear();
        for e in &s.entities {
            self.entities.insert(e.entity, (e.x, e.y));
        }
        self.last_seq = Some(s.sequence);
        Apply::Full
    }

    /// The one-shot private full (a per-connection baseline reset — see
    /// the `Private{snapshot}` docs in `game.proto`): applied
    /// UNCONDITIONALLY, outside the group stream's sequence logic.
    fn apply_private_full(&mut self, s: &gsb_demo::game::WorldSnapshot) {
        self.entities.clear();
        for e in &s.entities {
            self.entities.insert(e.entity, (e.x, e.y));
        }
        self.last_seq = Some(s.sequence);
    }
}
