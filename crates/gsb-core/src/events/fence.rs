//! A shard's half of the migration fence (`docs/EVENTS.md` §7.3): the
//! state that keeps every (publisher, identity, leg) FIFO intact while a
//! member crosses from shard A to shard B — with no buffer in the
//! router.
//!
//! ```text
//! source A                       router (registry)                target B
//!  MIGRATE: Migrate{mig} ───────────── shard FIFO ──────────────▶ CONTROL: install,
//!  TOMBSTONE bob → B                                              HOLD(bob, mig) ◀─┐
//!                                  ◀──────── Relocated{bob, B, mig} ───────────────┘
//!                                  place := B
//!                                  SEAL{bob, mig} ─▶ A's event mailbox
//!                                  (behind every leg sent to A)
//!                                  then direct ─────────────────▶ EVENTS: bob held
//!  EVENTS: legs for bob → tomb →
//!          Forward ───────────────────── shard FIFO ────────────▶ CONTROL: Forward → table
//!  SEAL → Fence{bob, mig} ────────────── shard FIFO ────────────▶ CONTROL: Fence → release
//! ```
//!
//! The SEAL rides A's event mailbox behind every leg the router sent A;
//! the FENCE rides the shard FIFO behind every leg A forwarded. B holds
//! the router's direct legs until the fence: per-publisher order holds.
//!
//! **The chain rule** (A → B → C before A's fence reached B): B still
//! waits for its incoming fence when the member leaves again, so its OWN
//! direct legs (held before it left, or arriving after) are newer than
//! what A still forwards — they stay pending at B; legs forwarded to B
//! are relayed to C at once; B sends its outgoing fence only after the
//! incoming one arrived (and the router's seal for B's own move).
//!
//! **Precedence** for a direct (router-sent) leg: an unsealed tombstone
//! (the router has not yet learnt the move away: the leg is older than
//! anything the new place gets) → the hold → the member → a sealed
//! tombstone → `events_world_not_here`. A forwarded leg skips the first
//! two: it is always older than the direct legs held beside it.
//!
//! **Bounds** (§20 E8, constants): a tombstone forwards for
//! [`TOMB_TTL`] ticks; a hold waits [`FENCE_TTL`] ticks for its fence and
//! then releases anyway (`events_fence_timeouts`); a hold keeps at most
//! [`HOLD_CAP`] legs (`events_fence_overflow`). Every one is counted.
//!
//! A single room never migrates: its tables stay empty and cost nothing.

use std::collections::{HashMap, VecDeque};

use crate::events::{Addressee, WorldPush};
use crate::metrics::EventCounts;

/// Ticks a tombstone hands on the legs that still reach the shard a
/// member left (`docs/EVENTS.md` §20 E8).
pub const TOMB_TTL: u64 = 64;
/// Ticks a hold waits for its fence before it releases anyway.
pub const FENCE_TTL: u64 = 64;
/// Legs one hold (or one tombstone's pending list) keeps.
pub const HOLD_CAP: usize = 256;

/// What a shard sends its neighbour for the fence (the shard actor turns
/// it into a `ShardMsg`).
#[derive(Debug)]
pub(crate) enum Relay {
    /// A WORLD leg for a member that moved to the target.
    Forward(WorldPush),
    /// Nothing more for `identity` comes from here (migration `mig`).
    Fence { identity: String, mig: u64 },
}

/// A member that left this shard: where its stragglers go.
#[derive(Debug)]
struct Tomb {
    /// The shard it moved to.
    to: usize,
    /// Its migration count out of here.
    mig_out: u64,
    /// The tick the tombstone expires at.
    until: u64,
    /// The incoming fence this shard still waited for when the member
    /// left again (the chain rule): the outgoing fence waits for it.
    awaiting_in: Option<u64>,
    /// The router's seal for `mig_out` arrived.
    sealed: bool,
    /// While `awaiting_in`: this shard's own direct legs for the member
    /// (newer than what the previous source still forwards).
    pending: Vec<WorldPush>,
}

/// A member that arrived: its direct legs wait for the source's fence.
#[derive(Debug)]
struct Hold {
    mig: u64,
    since: u64,
    held: Vec<WorldPush>,
}

/// One shard's fence state (see the module docs). Keyed by identity: the
/// router addresses WORLD legs by identity; anonymous members are never
/// addressed and never fenced.
#[derive(Debug, Default)]
pub(crate) struct Fences {
    tombs: HashMap<String, Tomb>,
    holds: HashMap<String, Hold>,
    /// The migration count of each identified member that migrated in
    /// (absent = 0: it joined here).
    migs: HashMap<String, u64>,
    /// Legs that entered in CONTROL (forwarded, or released by a fence)
    /// for this tick's EVENTS, ahead of the mailbox.
    pub(super) ready: VecDeque<WorldPush>,
    /// What to send the neighbours, in order.
    out: Vec<(usize, Relay)>,
}

/// Where a leg goes (the precedence of the module docs).
pub(super) enum Way {
    /// Into the table (a member here, or a room-wide leg).
    Admit(WorldPush),
    /// Nowhere: counted `events_world_not_here`.
    NotHere,
    /// Kept (held, pending, relayed, or counted lost by the fence).
    Kept,
}

impl Fences {
    /// The count the member's next migration out of here carries.
    pub(crate) fn next_mig(&self, identity: &str) -> u64 {
        self.migs.get(identity).copied().unwrap_or(0) + 1
    }

    /// The member left for shard `to` (its `Migrate` was sent with
    /// `mig`): a tombstone. A hold still waiting for its incoming fence
    /// becomes the tombstone's pending list (the chain rule).
    pub(crate) fn departed(&mut self, identity: &str, to: usize, mig: u64, tick: u64) {
        if identity.is_empty() {
            return;
        }
        self.migs.remove(identity);
        let (awaiting_in, pending) = match self.holds.remove(identity) {
            Some(h) => (Some(h.mig), h.held),
            None => (None, Vec::new()),
        };
        self.tombs.insert(
            identity.to_owned(),
            Tomb {
                to,
                mig_out: mig,
                until: tick + TOMB_TTL,
                awaiting_in,
                sealed: false,
                pending,
            },
        );
    }

    /// The member arrived with migration count `mig`; `hold` when the
    /// router took its `Relocated` (a seal — and so a fence — will come).
    pub(crate) fn arrived(&mut self, identity: &str, mig: u64, tick: u64, hold: bool) {
        if identity.is_empty() {
            return;
        }
        self.migs.insert(identity.to_owned(), mig);
        if hold {
            let h = self.holds.entry(identity.to_owned()).or_insert(Hold {
                mig,
                since: tick,
                held: Vec::new(),
            });
            h.mig = mig;
            h.since = tick;
        }
    }

    /// The member's membership here ended (a leave, a despawn): its
    /// migration count goes. A hold or tombstone runs its course.
    pub(crate) fn left(&mut self, identity: &str) {
        self.migs.remove(identity);
    }

    /// Route one leg. `member`: whether the addressee is a member here.
    pub(super) fn route(
        &mut self,
        p: WorldPush,
        forwarded: bool,
        member: impl FnOnce(&str) -> bool,
        c: &mut EventCounts,
    ) -> Way {
        let Addressee::Player(id) = &p.to else {
            return Way::Admit(p);
        };
        let id = id.as_str();
        if !forwarded {
            if self.tombs.get(id).is_some_and(|t| !t.sealed) {
                self.to_tomb(p, false, c);
                return Way::Kept;
            }
            if let Some(h) = self.holds.get_mut(id) {
                if h.held.len() >= HOLD_CAP {
                    c.events_fence_overflow += 1;
                } else {
                    h.held.push(p);
                    c.events_fence_held += 1;
                }
                return Way::Kept;
            }
        }
        if member(id) {
            return Way::Admit(p);
        }
        if self.tombs.contains_key(id) {
            self.to_tomb(p, forwarded, c);
            return Way::Kept;
        }
        Way::NotHere
    }

    /// A leg for a tombstoned identity: pending behind the incoming fence
    /// (a direct leg while the chain rule waits), else handed on.
    fn to_tomb(&mut self, p: WorldPush, forwarded: bool, c: &mut EventCounts) {
        let Addressee::Player(id) = &p.to else {
            return;
        };
        let Some(t) = self.tombs.get_mut(id.as_str()) else {
            return;
        };
        if !forwarded && t.awaiting_in.is_some() {
            if t.pending.len() >= HOLD_CAP {
                c.events_fence_overflow += 1;
            } else {
                t.pending.push(p);
                c.events_fence_held += 1;
            }
            return;
        }
        let to = t.to;
        self.out.push((to, Relay::Forward(p)));
    }

    /// CONTROL: a leg a neighbour forwarded (on the shard FIFO). Handed
    /// on at once when the member left here too (the chain rule — its
    /// place in the outgoing order is now, ahead of any fence this
    /// CONTROL sends); else it waits for this tick's EVENTS.
    pub(crate) fn forward_in(
        &mut self,
        p: WorldPush,
        member: impl FnOnce(&str) -> bool,
        c: &mut EventCounts,
    ) {
        let away = match &p.to {
            Addressee::Player(id) => self.tombs.contains_key(id.as_str()) && !member(id.as_str()),
            Addressee::Room => false,
        };
        if away {
            self.to_tomb(p, true, c);
        } else {
            self.ready.push_back(p);
        }
    }

    /// CONTROL: a neighbour's fence for `identity`'s migration `mig`.
    pub(crate) fn fence_in(&mut self, identity: &str, mig: u64, c: &mut EventCounts) {
        if self.holds.get(identity).is_some_and(|h| h.mig == mig) {
            self.release(identity, c);
            return;
        }
        if let Some(t) = self.tombs.get_mut(identity)
            && t.awaiting_in == Some(mig)
        {
            t.awaiting_in = None;
            let to = t.to;
            c.events_fence_released += t.pending.len() as u64;
            for p in std::mem::take(&mut t.pending) {
                self.out.push((to, Relay::Forward(p)));
            }
            if t.sealed {
                let out = t.mig_out;
                self.tombs.remove(identity);
                self.out.push((
                    to,
                    Relay::Fence {
                        identity: identity.to_owned(),
                        mig: out,
                    },
                ));
            }
            return;
        }
        c.events_fence_lost += 1;
    }

    /// EVENTS: the router's seal for `identity`'s migration `mig` out of
    /// here (behind every leg it sent here).
    pub(super) fn seal(&mut self, identity: &str, mig: u64, c: &mut EventCounts) {
        match self.tombs.get_mut(identity) {
            Some(t) if t.mig_out == mig && !t.sealed => {
                t.sealed = true;
                if t.awaiting_in.is_none() {
                    let to = t.to;
                    self.tombs.remove(identity);
                    self.out.push((
                        to,
                        Relay::Fence {
                            identity: identity.to_owned(),
                            mig,
                        },
                    ));
                }
            }
            _ => c.events_fence_lost += 1,
        }
    }

    /// CONTROL, once per tick: expired tombstones go (what they still
    /// kept pending is handed on, best effort); holds past `FENCE_TTL`
    /// release anyway.
    pub(crate) fn sweep(&mut self, tick: u64, c: &mut EventCounts) {
        if self.tombs.is_empty() && self.holds.is_empty() {
            return;
        }
        let dead: Vec<String> = self
            .tombs
            .iter()
            .filter(|(_, t)| t.until <= tick)
            .map(|(k, _)| k.clone())
            .collect();
        for id in dead {
            c.events_tombstones_expired += 1;
            if let Some(t) = self.tombs.remove(&id) {
                c.events_fence_released += t.pending.len() as u64;
                for p in t.pending {
                    self.out.push((t.to, Relay::Forward(p)));
                }
            }
        }
        let late: Vec<String> = self
            .holds
            .iter()
            .filter(|(_, h)| tick.saturating_sub(h.since) > FENCE_TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for id in late {
            c.events_fence_timeouts += 1;
            self.release(&id, c);
        }
    }

    /// Let a hold's legs go, in order, ahead of the mailbox.
    fn release(&mut self, identity: &str, c: &mut EventCounts) {
        let Some(h) = self.holds.remove(identity) else {
            return;
        };
        c.events_fence_released += h.held.len() as u64;
        self.ready.extend(h.held);
    }

    /// What to send the neighbours since the last call, in order.
    pub(crate) fn take_relays(&mut self) -> Vec<(usize, Relay)> {
        std::mem::take(&mut self.out)
    }
}
