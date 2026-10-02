//! The per-source handshake cap (BACKLOG D11): one source address holds
//! at most `per_source` of its door's handshake slots.
//!
//! - **The source.** An IPv4 address; an IPv6 address by its /64; an
//!   IPv4-mapped IPv6 address as its IPv4 address — the one rule
//!   [`gsb_core::source::Source`] holds, which the registry's per-source
//!   cap on unauthenticated connections (D12) counts by too.
//! - **Proven and unproven.** A TCP door's peer has completed the TCP
//!   handshake: its address is its own. A QUIC `Incoming` may come from a
//!   spoofed address until it proves it (a Retry token): an attacker
//!   could fill a victim's count with spoofed packets and get the victim
//!   refused. So an unproven source is counted apart from the same
//!   address proven, and an unproven connection over the cap is not
//!   refused but asked to prove its address (a stateless Retry, no
//!   slot): the real owner answers and comes back proven; a spoofer
//!   cannot. A real source can thus hold the cap twice on QUIC (once
//!   each way), never more.
//! - **No lock.** The table lives in the door's intake task — the one
//!   task that takes slots. A slot is released elsewhere (its handshake
//!   task, the accept loop, the close's drain), so its drop sends the
//!   source back through the intake's queue and the task drains it before
//!   each decision.
//! - **Bounded.** An entry exists only while its source holds a slot (it
//!   goes with the last one), and every slot is one of the door's `max`:
//!   the table never has more than `max` entries, the release queue never
//!   more than `max` keys — whatever the sources, spoofed QUIC ones
//!   included.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_channel::Receiver;
use gsb_core::source::Source;
use tracing::{debug, warn};

use crate::transport::intake::{Intake, Slot};

/// A source the per-source cap counts (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SourceKey {
    /// The IPv4 address, or the IPv6 /64.
    net: Source,
    /// A QUIC peer that has not proven its address yet.
    unproven: bool,
}

impl SourceKey {
    /// The source of a peer at `ip`; `proven`: its address is its own.
    pub(crate) fn of(ip: IpAddr, proven: bool) -> Self {
        Self {
            net: Source::of(ip),
            unproven: !proven,
        }
    }
}

impl fmt::Display for SourceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.net)?;
        if self.unproven {
            f.write_str(" (unproven)")?;
        }
        Ok(())
    }
}

/// A source's slots, and whether its spell at the cap was logged.
struct Held {
    slots: usize,
    warned: bool,
}

/// The intake task's table of held slots per source (module docs).
pub(crate) struct SourceTable {
    held: HashMap<SourceKey, Held>,
    released: Receiver<SourceKey>,
}

/// What the intake does with a new connection.
pub(crate) enum Admit {
    /// A slot: start its handshake.
    Slot(Slot),
    /// Refused at the door's bound (counted).
    Refused,
    /// Its source is at the per-source cap: refuse it — or, unproven
    /// (QUIC), ask it to prove its address — and count which
    /// ([`Intake::count_source_refusal`], [`Intake::count_source_retry`]).
    OverSource {
        /// The source has not proven its address (a Retry can).
        unproven: bool,
    },
}

impl SourceTable {
    /// The table of the intake task of `intake`'s door.
    pub(crate) fn new(intake: &Intake) -> Self {
        Self {
            held: HashMap::new(),
            released: intake.release_rx.clone(),
        }
    }

    /// Give back every slot released since the last decision.
    fn reclaim(&mut self) {
        while let Ok(key) = self.released.try_recv() {
            if let Entry::Occupied(mut e) = self.held.entry(key) {
                e.get_mut().slots -= 1;
                if e.get().slots == 0 {
                    e.remove();
                }
            }
        }
    }

    /// Sources holding a slot now.
    #[cfg(test)]
    pub(crate) fn sources(&mut self) -> usize {
        self.reclaim();
        self.held.len()
    }
}

impl Intake {
    /// Admit a new connection from `peer` (`proven`: its address is its
    /// own — always on TCP): the per-source cap first, when there is
    /// one, then the door's bound.
    pub(crate) fn admit(
        self: &Arc<Self>,
        table: &mut SourceTable,
        peer: IpAddr,
        proven: bool,
    ) -> Admit {
        let Some(cap) = self.per_source else {
            return self.try_slot().map_or(Admit::Refused, Admit::Slot);
        };
        table.reclaim();
        let key = SourceKey::of(peer, proven);
        if let Some(held) = table.held.get_mut(&key)
            && held.slots >= cap
        {
            if !std::mem::replace(&mut held.warned, true) {
                warn!(
                    door = self.kind,
                    source = %key,
                    cap,
                    "handshakes from one source at the per-source cap; refusing its new connections"
                );
            }
            debug!(door = self.kind, source = %key, "per-source handshake cap reached");
            return Admit::OverSource {
                unproven: key.unproven,
            };
        }
        let Some(mut slot) = self.try_slot() else {
            return Admit::Refused;
        };
        table
            .held
            .entry(key)
            .or_insert(Held {
                slots: 0,
                warned: false,
            })
            .slots += 1;
        slot.source = Some(key);
        Admit::Slot(slot)
    }

    /// Count a connection refused at the per-source cap.
    pub(crate) fn count_source_refusal(&self) {
        self.refused_per_source.fetch_add(1, Ordering::Relaxed);
    }

    /// Count an unproven QUIC connection asked, at the per-source cap, to
    /// prove its address.
    pub(crate) fn count_source_retry(&self) {
        self.retried_per_source.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
