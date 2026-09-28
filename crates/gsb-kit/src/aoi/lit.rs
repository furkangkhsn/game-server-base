//! [`LitAoiRoom`]: the AOI room with a per-viewer light (KIT-ARCHITECTURE
//! §10 "A9") — a game's opt-in filter INSIDE the visible neighbourhood
//! (a light cone, a line of sight, a stealth rule: the rule is the
//! game's, [`LitGame`]; the room is the seam).
//!
//! ## Delivery shape: a viewer with a light is its own group
//!
//! The AOI room's advantage is that a cell's packet is encoded once per
//! tick and shared by every member of the cell. A filtered viewer cannot
//! take that packet — it carries the records the viewer must not get —
//! so the group key grows a second arm ([`LitGroup`]):
//!
//! - **`Cell(c)`** — every player whose game gave it no light this tick:
//!   the AOI room's shared group, its packets, its one-shot private fulls
//!   and its keep-alives, unchanged (the hooks are the AOI room's own).
//! - **`Viewer(p)`** — a player with a light: a group of ONE. Its content
//!   is the lit subset of its neighbourhood (the same 3×3 / 27 cells),
//!   rebuilt every tick after the systems; its frames come from the set
//!   ledger the team room's delta mode uses (`crate::common::SetLedger`):
//!   a fresh group gets a FULL, an established one a DELTA against what
//!   the viewer holds — `removed` for every record that left its lit
//!   view (unlit now, or out of the neighbourhood, or gone), then the
//!   upserts of the records that entered it (a record lit again is a
//!   whole record) or whose wire value changed (on its due step, A10) —
//!   or nothing. Keep-alive: a fresh full. No `cell_exits` (a lit view is
//!   not a union of cells the client could compute) and no new wire
//!   element: the envelope and the client rules are `kit.proto`'s.
//!
//! **The security property.** A record the rule leaves unlit sends no
//! byte of itself to that viewer: the viewer's group frame is built from
//! its lit content only, its private frame carries only that content's
//! full, and it is never a member of a shared cell group while it has a
//! light. A record whose entity the room cannot resolve is unlit (fail
//! closed).
//!
//! ## Switching
//!
//! A player's arm is decided every tick. **Cell → Viewer:** its viewer
//! group is fresh, so its first frame is a full of the lit view (it
//! replaces the client's whole view). **Viewer → Cell:** its client
//! holds only the lit subset, so the AOI room's baseline for it is taken
//! back (a one-shot private full of the whole view rides the batch), and
//! a cell group that had no shared member at the previous tick is fresh
//! for the core — it is marked born, so its first packet is the full the
//! core expects of a fresh group.
//!
//! ## Cost model (only a game that lights pays; bounded)
//!
//! Per tick: one [`LitGame::light`] call per player; per viewer with a
//! light, one [`LitGame::lit`] call per record of its neighbourhood (`V`),
//! a set diff over `V`, and the encoding of its own changed records — its
//! frame is encoded for it alone. A viewer without a light costs one
//! `light` call and nothing else: the shared cell pieces are still
//! encoded once per cell. So with `F` lit viewers the per-tick extra is
//! `O(P + F·V)` predicate calls and `O(F·V)` diff work, and the extra
//! bytes are whatever the lit views differ in; the state is `O(F·V)`
//! (each lit viewer's held view) plus the kit's `wire id → entity` index
//! (`O(E)`, kept by this room only). `F = 0` is the AOI room plus `P`
//! `light` calls.
//!
//! **Not covered:** the sharded spatial composite (its borrowed records
//! have no entity on the viewer's shard) — a later item.

mod logic;
mod view;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use gsb_core::id::PlayerId;

use crate::aoi::AoiRoom;
use crate::common::{Baselines, SetLedger};
use crate::game::{LitGame, Wire};
use crate::space::CellSpace;

/// The lit AOI room's group key (module docs): a shared cell group, or
/// the one-member group of a viewer with a light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LitGroup<C> {
    /// The shared group of the cell's members without a light (the AOI
    /// room's group).
    Cell(C),
    /// The group of ONE viewer whose game gave it a light this tick.
    Viewer(PlayerId),
}

/// A viewer with a light: its lit view this tick and what its client
/// holds (the set ledger).
struct Viewer<W> {
    /// This tick's lit view: `wire id → wire value`.
    content: HashMap<u64, W>,
    /// What the viewer's client holds, and this step's full.
    ledger: SetLedger<W>,
}

// Not derived: a derive would demand `W: Default`.
impl<W> Default for Viewer<W> {
    fn default() -> Self {
        Self {
            content: HashMap::new(),
            ledger: SetLedger::default(),
        }
    }
}

/// The AOI room with a per-viewer light (module docs).
pub struct LitAoiRoom<G: LitGame, S: CellSpace<Wire<G>>> {
    /// The AOI room: the world's bookkeeping, the shared cell groups and
    /// every session hook.
    room: AoiRoom<G, S>,
    /// The players with a light this tick (bounded by the players).
    viewers: HashMap<PlayerId, Viewer<Wire<G>>>,
    /// Which viewer SESSIONS hold a baseline of their own view — the
    /// one-shot private full of a viewer group (a resume, a dropped
    /// batch); the fresh group's full establishes it.
    baselines: Baselines<()>,
    /// The cells with a shared member at the previous tick / this tick:
    /// a cell group that had none is fresh for the core.
    shared: HashSet<S::Cell>,
    shared_now: HashSet<S::Cell>,
    /// Viewer records encoded since the last poll.
    encoded: u64,
}

impl<G: LitGame, S: CellSpace<Wire<G>>> AoiRoom<G, S> {
    /// This room with the game's per-viewer light ([`LitAoiRoom`]).
    #[must_use]
    pub fn lit(mut self) -> LitAoiRoom<G, S> {
        self.book.index_entities();
        LitAoiRoom {
            room: self,
            viewers: HashMap::new(),
            baselines: Baselines::default(),
            shared: HashSet::new(),
            shared_now: HashSet::new(),
            encoded: 0,
        }
    }
}

impl<G: LitGame, S: CellSpace<Wire<G>>> LitAoiRoom<G, S> {
    /// Build a lit AOI room running `game` over the cell space `space`
    /// (the AOI room's builders: [`AoiRoom::lit`]).
    #[must_use]
    pub fn with_game(game: G, space: S) -> Self {
        AoiRoom::with_game(game, space).lit()
    }

    /// The game this room runs.
    pub fn game(&self) -> &G {
        self.room.game()
    }

    /// The game this room runs, for configuration after construction.
    pub fn game_mut(&mut self) -> &mut G {
        self.room.game_mut()
    }
}
