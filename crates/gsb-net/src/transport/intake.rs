//! Handshakes off the accept loop (BACKLOG B31).
//!
//! A door whose connections need a handshake before they are sessions
//! (WebSocket upgrade, TLS, QUIC) used to run it INSIDE `accept`, and the
//! server's accept loop awaits one accept at a time: the handshakes ran
//! in series. One socket that never sent its upgrade held the whole door
//! for the 10 s handshake deadline, a connect storm overflowed the
//! backlog, and a failed handshake backed the loop off.
//!
//! Now the door's own intake task accepts the raw connection and hands
//! it at once to a per-connection handshake task; the finished endpoint
//! reaches `accept` through a queue. The shape:
//!
//! ```text
//! intake task ──raw accept──▶ slot? ──yes──▶ handshake task: ONE await,
//!   (one per door)             │             door ⊃ deadline ⊃ handshake
//!                              no                    │ Ok
//!                              ▼                     ▼
//!                  refuse (close) + count     queue (endpoint + its slot)
//!                                                    │
//! server accept loop ◀── Listener::accept ◀──────────┘ (slot released)
//! ```
//!
//! - **The bound.** A slot is held from the raw accept until the accept
//!   loop takes the endpoint, so "in flight" covers a handshake running
//!   and one finished but not yet taken. Over the bound a new connection
//!   is refused on the spot — the socket closed, the refusal counted —
//!   never queued. The queue itself can never outgrow the slots (every
//!   entry carries one), which is why it is crossbeam's unbounded flavor:
//!   `bounded(max)` would preallocate every slot's entry up front, and
//!   the composition root sets `max` from the pre-auth cap (tens of
//!   thousands by default).
//! - **The door.** Every await here runs under the listener's [`Door`]:
//!   `close` ends the raw accept, cuts every handshake in flight (its
//!   socket dropped), and drops what is queued — B16's contract, now for
//!   the handshakes that are no longer inside `accept`. Both are counted
//!   (B74: `handshakes_cut_closed`, `handshakes_unaccepted_closed`), and
//!   the intake task's last sample waits for them (`close::settle`).
//! - **Per source (D11, opt-in).** The bound above is the door's; with a
//!   per-source cap one source address (IPv4 address, IPv6 /64) holds at
//!   most that many slots, so no single source can take the door's
//!   whole bound. The table of held slots per source lives in the intake
//!   task alone (`source`); a released slot reaches it through a queue
//!   it drains before each decision. Unset (the default) = no table, no
//!   queue traffic, the door as it was.
//! - **No second source.** The accept loop still awaits one thing
//!   (`accept`, which waits on the queue); the intake task awaits the raw
//!   accept; each handshake task awaits exactly one future.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crossbeam_channel::{Receiver, Sender};
use tokio::sync::Notify;
use tracing::warn;

use crate::transport::{Door, Endpoint};

mod close;
mod handshake;
mod source;
pub(crate) use source::{Admit, SourceKey, SourceTable};
mod stats;
pub use stats::HandshakeStats;
mod tcp;
pub(crate) use tcp::run_tcp_intake;

/// The bound on a door's handshakes in flight when the transport is
/// built directly (tests, embedders). The server sets its own from the
/// pre-auth cap (`docs/SECURITY.md` §4).
pub const DEFAULT_MAX_PENDING_HANDSHAKES: usize = 1024;

/// A door's intake state, shared by its intake task, its handshake
/// tasks and its listener handle.
pub(crate) struct Intake {
    /// For the logs: "WebSocket", "TLS", "QUIC".
    kind: &'static str,
    door: Door,
    max: usize,
    held: AtomicUsize,
    /// The per-source cap (D11; `None` = no per-source limit).
    per_source: Option<usize>,
    /// Released slots' sources, for the intake task's table (`source`):
    /// every held slot sends at most one, so it never outgrows the slots.
    release_tx: Sender<SourceKey>,
    release_rx: Receiver<SourceKey>,
    /// Set by the first refusal of a saturated spell (one warning per
    /// spell, not one per refused connection); cleared by the next slot.
    saturated: AtomicBool,
    queue_tx: Sender<Ready>,
    queue_rx: Receiver<Ready>,
    /// Wakes the accept waiting on an empty queue.
    ready: Notify,
    /// Handshake tasks still running, their counting included (B74).
    live: AtomicUsize,
    /// Wakes the intake task's settle wait: the last slot released, or
    /// the last handshake task ended (B74, `close::settle`).
    quiet: Notify,
    completed: AtomicU64,
    refused: AtomicU64,
    timed_out: AtomicU64,
    failed: AtomicU64,
    /// What the door's close cut (handshakes in flight) and dropped
    /// (finished handshakes still queued) — B74.
    cut: AtomicU64,
    unaccepted: AtomicU64,
    /// Connections refused at the per-source cap (D11), and QUIC
    /// connections asked to prove their address there (a Retry).
    refused_per_source: AtomicU64,
    retried_per_source: AtomicU64,
}

/// A finished handshake waiting for the accept loop, with its slot.
struct Ready {
    endpoint: Endpoint,
    _slot: Slot,
}

/// One of the door's `max` handshake slots; released on drop — and, when
/// the door has a per-source cap, given back to its source's count.
pub(crate) struct Slot {
    intake: Arc<Intake>,
    source: Option<SourceKey>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // The source first: the intake task reclaims it at its next
        // decision. Never fails: the intake holds the receiver.
        if let Some(key) = self.source.take() {
            let _ = self.intake.release_tx.send(key);
        }
        if self.intake.held.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.intake.quiet.notify_one();
        }
    }
}

impl Intake {
    /// A door's intake with `max` slots (at least one).
    #[cfg(test)]
    pub(crate) fn new(kind: &'static str, max: usize) -> Arc<Self> {
        Self::with_source_cap(kind, max, None)
    }

    /// A door's intake with `max` slots (at least one), at most
    /// `per_source` of them held by one source (D11; `None` or `0` = no
    /// per-source cap).
    pub(crate) fn with_source_cap(
        kind: &'static str,
        max: usize,
        per_source: Option<usize>,
    ) -> Arc<Self> {
        let (queue_tx, queue_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        Arc::new(Self {
            kind,
            door: Door::new(),
            max: max.max(1),
            held: AtomicUsize::new(0),
            per_source: per_source.filter(|&n| n > 0),
            release_tx,
            release_rx,
            saturated: AtomicBool::new(false),
            queue_tx,
            queue_rx,
            ready: Notify::new(),
            live: AtomicUsize::new(0),
            quiet: Notify::new(),
            completed: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            cut: AtomicU64::new(0),
            unaccepted: AtomicU64::new(0),
            refused_per_source: AtomicU64::new(0),
            retried_per_source: AtomicU64::new(0),
        })
    }

    /// The listener's door (the intake task's raw accept runs under it).
    pub(crate) fn door(&self) -> &Door {
        &self.door
    }

    /// Take a slot for a new connection, or count its refusal.
    pub(crate) fn try_slot(self: &Arc<Self>) -> Option<Slot> {
        let taken = self
            .held
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max).then_some(n + 1)
            })
            .is_ok();
        if taken {
            self.saturated.store(false, Ordering::Relaxed);
            return Some(Slot {
                intake: Arc::clone(self),
                source: None,
            });
        }
        self.refused.fetch_add(1, Ordering::Relaxed);
        if !self.saturated.swap(true, Ordering::Relaxed) {
            warn!(
                door = self.kind,
                max = self.max,
                "handshakes in flight at the bound; refusing new connections"
            );
        }
        None
    }
}

/// The listener handle's share of an [`Intake`]: dropping the last
/// handle closes the door, like dropping a listener socket did when the
/// socket lived in the handle (the intake task holds the socket now).
pub(crate) struct IntakeHandle(pub(crate) Arc<Intake>);

impl Drop for IntakeHandle {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
pub(crate) mod tests;
