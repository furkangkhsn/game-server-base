//! [`SnapshotBudget`]: a full-snapshot room's answer to a member whose
//! path is limited (BACKLOG B103, KIT-ARCHITECTURE §10 "B103") — the
//! opt-in building block behind `GameLogic::ship_snapshot`.
//!
//! The transport measures (rUDP's pacing, QUIC's congestion window) and
//! the core carries the result to the room as a per-member byte budget
//! (`TickCtx::budget`). The group frame is encoded ONCE and shared by
//! every member of the group, so the one thing a room can decide per
//! member without losing that is *how often* the member gets it: a
//! member whose budget fits the frame gets every one (full rate); a
//! member whose budget does not gets the frame when its credit — the
//! bytes its path drained since the last frame it got — covers it. The
//! rest of its frames are withheld (the core counts them:
//! `snapshots_withheld`). Its private frame (acks, RPC answers, the
//! session payload) is never thinned.
//!
//! **Full snapshots only.** A withheld full is healed by the next one
//! the member gets; a withheld delta would leave a gap the client cannot
//! see. So the open, sector (PVS) and plain sharded rooms offer it
//! (`with_snapshot_budget`); the delta rooms (AOI, team delta, sharded ×
//! spatial / team) do not.
//!
//! **Bounded staleness.** However small the budget, a member gets at
//! least one frame in every [`SnapshotBudget::DEFAULT_HELD_MAX`]` + 1`
//! frames offered (16: A10's coarsest class, `Ticks16`) — the frame it
//! is then sent over its budget is counted (`snapshot_budget_forced`,
//! the logic-counter seam): the transport may drop part of it, and the
//! operator sees why.
//!
//! **Credit.** Each frame offered adds the member's budget for every
//! room step since its last offer — a group that had no frame for a
//! while (unchanged) still drained the path — capped at the frame plus
//! one step's budget: a member that was quiet a while gets one frame at
//! once, never a burst. A step is the room's own: the block learns its
//! stride in global ticks (a room stepping every k-th tick) as the
//! smallest gap between two offers, so a slower room is not credited
//! twice. A frame sent by the staleness bound spends all the credit
//! there was. The table is shard-local soft
//! state, keyed by the stable player: a member no longer asked (left,
//! crossed to another shard, back to an open path) is swept after
//! [`SWEEP_TICKS`] ticks, and one asked again starts with no credit.

#[cfg(test)]
mod rooms;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use gsb_core::id::PlayerId;
use gsb_core::metrics::{LogicCounter, LogicCounters};

/// How long (global ticks) a member's credit outlives its last offer.
pub const SWEEP_TICKS: u64 = 64;

const FORCED: LogicCounter = LogicCounter::sum(
    "snapshot_budget_forced",
    "Group frames SnapshotBudget sent over a limited member's budget because it had withheld the most frames in a row it may, cumulative.",
);

/// One limited member's credit.
#[derive(Debug, Clone, Copy)]
struct Credit {
    /// Bytes its path drained since the last frame it got (capped).
    bytes: u64,
    /// The (global) tick of its last offer.
    last: u64,
    /// Frames withheld in a row.
    held: u32,
}

/// Per-member frame-rate thinning under a path budget (see the module
/// docs). A room that opts in calls [`Self::admit`] from its
/// `GameLogic::ship_snapshot` and reports [`Self::counters`].
#[derive(Debug, Clone)]
pub struct SnapshotBudget {
    members: HashMap<PlayerId, Credit>,
    held_max: u32,
    swept_at: u64,
    forced: u64,
    /// The tick of the latest offer to any member, and the room's step
    /// in global ticks: the smallest gap seen between two (0: unknown).
    step_tick: Option<u64>,
    stride: u64,
}

impl Default for SnapshotBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotBudget {
    /// The most frames withheld in a row by default: a member gets at
    /// least one frame in every 16 offered.
    pub const DEFAULT_HELD_MAX: u32 = 15;

    /// The default building block.
    pub fn new() -> Self {
        Self::with_held_max(Self::DEFAULT_HELD_MAX)
    }

    /// At most `held_max` frames withheld in a row (`0`: never withhold —
    /// the block then only counts).
    pub fn with_held_max(held_max: u32) -> Self {
        Self {
            members: HashMap::new(),
            held_max,
            swept_at: 0,
            forced: 0,
            step_tick: None,
            stride: 0,
        }
    }

    /// Ship this tick's group frame of `bytes` to `player`, whose path
    /// takes `budget` bytes per tick? `tick` is the tick context's.
    pub fn admit(&mut self, player: PlayerId, tick: u64, bytes: usize, budget: usize) -> bool {
        self.sweep(tick);
        self.learn_stride(tick);
        let stride = self.stride.max(1);
        let (bytes, budget) = (bytes as u64, budget as u64);
        let c = self.members.entry(player).or_insert(Credit {
            bytes: 0,
            last: tick.saturating_sub(stride),
            held: 0,
        });
        let elapsed = tick.saturating_sub(c.last).div_ceil(stride).max(1);
        c.last = tick;
        c.bytes = c
            .bytes
            .saturating_add(budget.saturating_mul(elapsed))
            .min(bytes.saturating_add(budget));
        if c.bytes >= bytes {
            c.bytes -= bytes;
            c.held = 0;
            return true;
        }
        if c.held >= self.held_max {
            c.bytes = 0;
            c.held = 0;
            self.forced += 1;
            return true;
        }
        c.held += 1;
        false
    }

    /// The block's counter (`snapshot_budget_forced`), for the room's
    /// `GameLogic::logic_counters`.
    pub fn counters(&self, out: &mut LogicCounters) {
        out.put(&FORCED, self.forced);
    }

    /// Frames sent over budget by the staleness bound, cumulative.
    pub fn forced(&self) -> u64 {
        self.forced
    }

    /// Members with credit (asked within the last [`SWEEP_TICKS`]).
    pub fn tracked(&self) -> usize {
        self.members.len()
    }

    /// A room's seat of the block (`None`: the room did not opt in —
    /// every frame ships, as always).
    pub(crate) fn gate(
        seat: &mut Option<Self>,
        player: PlayerId,
        tick: u64,
        bytes: usize,
        budget: usize,
    ) -> bool {
        seat.as_mut()
            .is_none_or(|b| b.admit(player, tick, bytes, budget))
    }

    fn learn_stride(&mut self, tick: u64) {
        match self.step_tick {
            Some(prev) if tick > prev => {
                let gap = tick - prev;
                self.stride = if self.stride == 0 {
                    gap
                } else {
                    self.stride.min(gap)
                };
                self.step_tick = Some(tick);
            }
            Some(_) => {}
            None => self.step_tick = Some(tick),
        }
    }

    fn sweep(&mut self, tick: u64) {
        if tick < self.swept_at.saturating_add(SWEEP_TICKS) {
            return;
        }
        self.swept_at = tick;
        self.members
            .retain(|_, c| tick.saturating_sub(c.last) <= SWEEP_TICKS);
    }
}
