//! Remote effects in the tick (`docs/CROSS-SHARD.md` §2–§4): phase 0d
//! applies the effects addressed to this shard, phase 3b sends the ones
//! its hooks emitted.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::{debug, info, warn};

use crate::shard::actor::ShardActor;
use crate::shard::*;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Phase 0d — EFFECTS IN (after the CONTROL drain, before any of this
    /// tick's input): apply every received effect whose tick has come.
    ///
    /// Order: an effect emitted at the origin's tick `N` applies here at
    /// tick `N + 1` — the Migrate install gate's one-tick alignment, and
    /// for the same reason: shard tick bodies interleave, so without the
    /// gate the effects of one origin tick would split across two of
    /// this shard's ticks by scheduling luck. Due effects then apply
    /// sorted by `(source, origin, seq)` — a pure function of the set,
    /// so every run and every shard agree on who hit first (CROSS-SHARD
    /// §4 layer 3; no lock, no negotiation).
    pub(crate) fn phase_effects_in(&mut self, tick: u64) {
        self.effects.out.begin_tick(tick);
        self.effects.fold_outbox();
        // The forwarding table forgets migrations older than its TTL.
        if !self.effects.forwarded.is_empty() {
            self.effects.forwarded.retain(|_, (_, until)| *until > tick);
        }
        if self.effects.pending.is_empty() {
            return;
        }
        let mut due = std::mem::take(&mut self.effects.pending);
        let shard_count = self.effects.windows.len();
        let epoch = self.effects.out.epoch;
        let stats = &mut self.effects.stats;
        due.retain(|e| {
            if e.id.epoch != epoch || e.id.origin >= shard_count {
                stats.foreign += 1;
                false
            } else if e.at_tick > tick + EFFECT_MAX_AGE_TICKS {
                // Stamped implausibly far ahead of this shard's clock:
                // not an early arrival, a broken sender.
                stats.foreign += 1;
                false
            } else {
                true
            }
        });
        // Early arrivals (this shard is behind their origin) wait a tick.
        let (due, early): (Vec<_>, Vec<_>) = due.into_iter().partition(|e| tick > e.at_tick);
        self.effects.pending = early;
        let mut due = due;
        due.sort_unstable_by_key(|e| (e.source, e.id.origin, e.id.seq));
        for mut effect in due {
            let book = &mut self.effects;
            if tick - effect.at_tick > EFFECT_MAX_AGE_TICKS {
                book.stats.expired += 1;
                continue;
            }
            // Handed on since the sender last saw it lent from here
            // (or it is the doomed copy still in this world until the
            // migration's despawn): forward to the new owner.
            if let Some(&(to, _)) = book.forwarded.get(&effect.target) {
                if effect.hops >= EFFECT_MAX_HOPS {
                    book.stats.dropped_hops += 1;
                    continue;
                }
                effect.hops += 1;
                book.stats.forwarded += 1;
                book.out.queue.push((to, effect));
                continue;
            }
            match book.admit(effect.id) {
                Seen::Fresh => {}
                Seen::Duplicate | Seen::TooOld => {
                    book.stats.duplicates += 1;
                    continue;
                }
            }
            let mut seam = book.seam(&self.border, &self.lenders);
            let outcome = self
                .logic
                .apply_remote_effect(&mut self.world, tick, &effect, &mut seam);
            let stats = &mut self.effects.stats;
            match outcome {
                EffectOutcome::Applied => stats.applied += 1,
                EffectOutcome::Rejected => stats.rejected += 1,
                EffectOutcome::NoTarget => {
                    stats.orphaned += 1;
                    debug!(
                        room = %self.config.id,
                        shard = self.index,
                        target = effect.target,
                        "remote effect orphaned: no such entity here"
                    );
                }
            }
        }
    }

    /// Phase 3b — EFFECTS OUT (after the systems): send the retry buffer
    /// (oldest first), then this tick's emissions and forwards, each to
    /// its authority's link.
    ///
    /// Full-channel policy: RETRY NEXT TICK, bounded. A hit is gameplay —
    /// a silently lost one is a bug — so a full neighbour inbox (a
    /// transient stall) does not drop it; the effect waits in a buffer of
    /// at most [`EFFECT_RETRY_CAP`] entries, keeps its stamp, and expires
    /// after [`EFFECT_MAX_AGE_TICKS`] like anywhere else. Overflow and a
    /// closed link drop and count. No await, no unbounded queue.
    pub(crate) fn phase_effects_out(&mut self, tick: u64) {
        let book = &mut self.effects;
        book.fold_outbox();
        if book.retry.is_empty() && book.out.queue.is_empty() {
            return;
        }
        let retry = std::mem::take(&mut book.retry);
        let fresh = std::mem::take(&mut book.out.queue);
        for (link, effect) in retry.into_iter().chain(fresh) {
            if tick.saturating_sub(effect.at_tick) > EFFECT_MAX_AGE_TICKS {
                book.stats.expired += 1;
                continue;
            }
            let Some(l) = self.links.get_mut(link) else {
                book.stats.dropped_closed += 1;
                continue;
            };
            match l.send(ShardMsg::RemoteEffect(effect)) {
                Ok(()) => book.stats.sent += 1,
                // The link hands back exactly the message it refused.
                Err(LinkFull::Full {
                    msg: ShardMsg::RemoteEffect(effect),
                }) => {
                    if book.retry.len() < EFFECT_RETRY_CAP {
                        book.stats.retried += 1;
                        book.retry.push_back((link, effect));
                    } else {
                        book.stats.dropped_full += 1;
                        warn!(
                            room = %self.config.id,
                            shard = self.index,
                            neighbor = link,
                            "remote effect dropped: the neighbour's inbox \
                             stayed full and the retry buffer is at its cap"
                        );
                    }
                }
                // Closed: this peer incarnation is gone (the room death
                // watcher owns that story).
                Err(_) => book.stats.dropped_closed += 1,
            }
        }
    }

    /// The ~1 s summary line of the effect counters (cumulative; logged
    /// only when something moved since the last line, so a shard that
    /// never looks across the seam logs nothing).
    pub(crate) fn log_effect_summary(&mut self) {
        let s = self.effects.stats;
        if s == self.effects.logged {
            return;
        }
        self.effects.logged = s;
        info!(
            room = %self.config.id,
            shard = self.index,
            emitted = s.emitted,
            refused = s.refused,
            sent = s.sent,
            retried = s.retried,
            dropped_full = s.dropped_full,
            dropped_closed = s.dropped_closed,
            expired = s.expired,
            received = s.received,
            applied = s.applied,
            rejected = s.rejected,
            orphaned = s.orphaned,
            duplicates = s.duplicates,
            forwarded = s.forwarded,
            dropped_hops = s.dropped_hops,
            foreign = s.foreign,
            "remote_effect_summary"
        );
    }
}
