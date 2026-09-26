//! Which group's view each player's SESSION holds a baseline for — the
//! one-shot private full's decision, shared by every delta room (the
//! team rooms key it by team, the AOI rooms by cell) — and, since F11,
//! what a fan-out DROP of a batch took from it.
//!
//! **The drop (F11).** A baseline is recorded when the frame that
//! establishes it is WRITTEN (the group's own full, or the one-shot
//! private full). When the core reports the batch dropped
//! (`GameLogic::on_batch_dropped`) and that batch carried view content
//! — the group frame, or the one-shot full — the client may hold no
//! baseline (a lost full: it drops every delta until the next full) or a
//! stale one (a lost delta: its records stay stale, its removals linger
//! as ghosts, until the next full). Either way the entry is taken back,
//! so the player is owed a one-shot full again: the view heals on the
//! next tick instead of the next keep-alive. A drop of a batch without
//! view content (an ack, RPC answers, the session payload alone) leaves
//! the baseline alone.
//!
//! **The storm bound.** A connection whose channel stays full would
//! otherwise be owed — and sent, and dropped — a full on every tick. So
//! each drop that takes a baseline pushes the NEXT re-send back:
//! `1, 2, 4, 8, 16` steps after the 1st … 5th such drop, then
//! [`RESEND_WAIT_MAX`] steps after every later one — at most six
//! drop-triggered fulls in the first 63 steps of a storm, then one per
//! 32 — whatever the delivery pattern (a channel that drops every other
//! batch escalates the same way). A drop while the player is already
//! owed one (the re-send is waiting) takes nothing and pushes nothing.
//! The pacing ends once the player's baseline has stood for
//! [`RESEND_WAIT_MAX`] steps past its last re-send slot with no drop
//! taking it: the next drop re-sends at once again. A group full (a
//! keep-alive) still baselines a waiting player — the pacing only holds
//! back the private one. The pending re-send is ONE missing entry per
//! player, never a queue.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::Hash;

use gsb_core::id::PlayerId;

/// The longest wait (steps) between two drop-triggered one-shot fulls
/// of a player whose batches keep being dropped.
pub(crate) const RESEND_WAIT_MAX: u64 = 32;

/// A player whose baseline a drop took, and when it may be re-sent.
#[derive(Debug, Clone, Copy)]
struct Lost {
    /// Drops that took a baseline since the pacing began.
    drops: u32,
    /// The first step a one-shot full may go out again.
    not_before: u64,
}

/// See the module docs. Bounded by the players: an entry is dropped on
/// leave and on resume (a resumed session has no baseline).
pub(crate) struct Baselines<K> {
    held: HashMap<PlayerId, K>,
    /// The drop pacing (F11): players whose baseline a fan-out drop took
    /// recently (module docs). Empty unless batches drop.
    lost: HashMap<PlayerId, Lost>,
}

// Not derived: a derive would demand `K: Default`.
impl<K> Default for Baselines<K> {
    fn default() -> Self {
        Self {
            held: HashMap::new(),
            lost: HashMap::new(),
        }
    }
}

/// The wait after the `drops`-th drop that took a baseline.
fn wait(drops: u32) -> u64 {
    (1u64 << drops.saturating_sub(1).min(5)).min(RESEND_WAIT_MAX)
}

impl<K: Copy + Eq + Hash> Baselines<K> {
    /// Whether `player`, now in `group` on step `now`, is owed a one-shot
    /// private full: it has no baseline for that group's view (a join, a
    /// resume, a group change, a dropped baseline) and the group's own
    /// frame this step, which precedes the private frame in the batch,
    /// was not a full (`group_full`, asked only then). Records the
    /// baseline when it is established; a drop-triggered re-send that is
    /// still waiting (module docs) is not owed yet.
    pub(crate) fn owed(
        &mut self,
        player: PlayerId,
        group: K,
        now: u64,
        group_full: impl FnOnce() -> bool,
    ) -> bool {
        if self.held.get(&player) == Some(&group) {
            // The baseline stands (a drop would have taken the entry
            // back); long enough past the last re-send slot, the pacing
            // is over. The quiet path pays one `is_empty` probe.
            if !self.lost.is_empty()
                && let Entry::Occupied(lost) = self.lost.entry(player)
                && now >= lost.get().not_before.saturating_add(RESEND_WAIT_MAX)
            {
                lost.remove();
            }
            return false;
        }
        if group_full() {
            self.held.insert(player, group);
            return false;
        }
        if self.lost.get(&player).is_some_and(|l| now < l.not_before) {
            return false;
        }
        self.held.insert(player, group);
        true
    }

    /// `player`'s batch of step `now` was dropped; `view` = it carried
    /// view content (the group frame or a one-shot full). Takes the
    /// baseline back and paces the re-send (module docs).
    pub(crate) fn dropped(&mut self, player: PlayerId, now: u64, view: bool) {
        if !view || self.held.remove(&player).is_none() {
            return;
        }
        let lost = self.lost.entry(player).or_insert(Lost {
            drops: 0,
            not_before: 0,
        });
        lost.drops = lost.drops.saturating_add(1);
        lost.not_before = now.saturating_add(wait(lost.drops));
    }

    /// `player`'s session ended or restarted: it holds no baseline, and
    /// the new session starts unpaced.
    pub(crate) fn forget(&mut self, player: PlayerId) {
        self.held.remove(&player);
        self.lost.remove(&player);
    }

    /// Whether `player` holds a baseline (for tests).
    #[cfg(test)]
    pub(crate) fn holds(&self, player: PlayerId) -> bool {
        self.held.contains_key(&player)
    }

    /// How many sessions hold a baseline (the table's bound, for tests).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.held.len()
    }

    /// How many sessions are paced (the drop table's bound, for tests).
    #[cfg(test)]
    pub(crate) fn paced(&self) -> usize {
        self.lost.len()
    }
}

#[cfg(test)]
mod tests;
