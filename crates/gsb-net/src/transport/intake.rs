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
//!   the handshakes that are no longer inside `accept`.
//! - **No second source.** The accept loop still awaits one thing
//!   (`accept`, which waits on the queue); the intake task awaits the raw
//!   accept; each handshake task awaits exactly one future.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::transport::{Door, Endpoint};

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
    /// Set by the first refusal of a saturated spell (one warning per
    /// spell, not one per refused connection); cleared by the next slot.
    saturated: AtomicBool,
    queue_tx: Sender<Ready>,
    queue_rx: Receiver<Ready>,
    /// Wakes the accept waiting on an empty queue.
    ready: Notify,
    completed: AtomicU64,
    refused: AtomicU64,
    timed_out: AtomicU64,
    failed: AtomicU64,
}

/// A finished handshake waiting for the accept loop, with its slot.
struct Ready {
    endpoint: Endpoint,
    _slot: Slot,
}

/// One of the door's `max` handshake slots; released on drop.
pub(crate) struct Slot(Arc<Intake>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.held.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Intake {
    /// A door's intake with `max` slots (at least one).
    pub(crate) fn new(kind: &'static str, max: usize) -> Arc<Self> {
        let (queue_tx, queue_rx) = crossbeam_channel::unbounded();
        Arc::new(Self {
            kind,
            door: Door::new(),
            max: max.max(1),
            held: AtomicUsize::new(0),
            saturated: AtomicBool::new(false),
            queue_tx,
            queue_rx,
            ready: Notify::new(),
            completed: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            failed: AtomicU64::new(0),
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
            return Some(Slot(Arc::clone(self)));
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

    /// Run one connection's handshake in its own task: ONE awaited
    /// future — the handshake under its deadline, under the door.
    pub(crate) fn spawn<F>(
        self: &Arc<Self>,
        slot: Slot,
        peer: SocketAddr,
        deadline: Duration,
        handshake: F,
    ) where
        F: Future<Output = io::Result<Endpoint>> + Send + 'static,
    {
        let intake = Arc::clone(self);
        tokio::spawn(async move {
            let deadlined = async { Ok(tokio::time::timeout(deadline, handshake).await) };
            // Each count is made once the slot has moved on (queued or
            // released), so a reader of the counters never sees it early.
            match intake.door.admit(deadlined).await {
                Ok(Ok(Ok(endpoint))) => {
                    debug!(door = intake.kind, %peer, "handshake completed");
                    intake.hand_over(Ready {
                        endpoint,
                        _slot: slot,
                    });
                    intake.completed.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Ok(Err(e))) => {
                    drop(slot);
                    intake.failed.fetch_add(1, Ordering::Relaxed);
                    warn!(door = intake.kind, %peer, error = %e, "handshake failed; closing");
                }
                Ok(Err(_)) => {
                    drop(slot);
                    intake.timed_out.fetch_add(1, Ordering::Relaxed);
                    warn!(door = intake.kind, %peer, timeout = ?deadline, "handshake timed out; closing");
                }
                Err(_) => debug!(door = intake.kind, %peer, "door closed; handshake cut"),
            }
        });
    }

    /// Queue a finished endpoint for the accept loop. Never full (the
    /// slots bound it); a close that raced the handshake drops it again.
    fn hand_over(&self, ready: Ready) {
        if self.queue_tx.try_send(ready).is_ok() {
            self.ready.notify_one();
        }
        if self.door.is_closed() {
            self.drain();
        }
    }

    /// The next finished endpoint (`Listener::accept`), or the closed
    /// error once the door is closed.
    pub(crate) async fn next(self: Arc<Self>) -> io::Result<Endpoint> {
        let queued = async {
            loop {
                if let Ok(Ready { endpoint, .. }) = self.queue_rx.try_recv() {
                    return Ok(endpoint);
                }
                // `notify_one` stores a wake-up when nobody waits yet,
                // so an endpoint queued between the check and this wait
                // is not missed.
                self.ready.notified().await;
            }
        };
        self.door.admit(queued).await
    }

    /// Close the door: the raw accept, every handshake in flight and the
    /// pending accept end, and what is queued is dropped.
    pub(crate) fn close(&self) {
        self.door.close();
        self.drain();
    }

    fn drain(&self) {
        while self.queue_rx.try_recv().is_ok() {}
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
