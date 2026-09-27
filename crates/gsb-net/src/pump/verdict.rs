//! A writer's road to the connection actor's mailbox: ONE slot of it,
//! reserved when the writer is born, so its verdict never waits for
//! room. Shared by the stream writer pump (the write stall) and the rUDP
//! writer (the reliable band's death — B66: before, its notice was a
//! `try_send` that a full mailbox refused, and the close was booked as
//! `outbound_dead`).
//!
//! Why a reserved slot: the verdict must be IN the mailbox before the
//! outbound channel closes — the close wakes the actor with a failed send
//! and it looks in its mailbox for the reason right then, synchronously
//! (`adopt_pending_close`), and leaves. Under overload that mailbox is
//! FULL at exactly that moment: the actor is parked on the outbound
//! channel this pump stopped draining, and the reader keeps queueing the
//! client's frames behind it. A `try_send` then fails, and a verdict
//! deferred past the close arrives after the actor has looked and gone —
//! the close is booked as `outbound_dead` (the 10k measurement: 67-172
//! per run). A slot taken from the mailbox's own capacity at birth, when
//! the mailbox is empty, makes the post synchronous and infallible
//! whatever the queue looks like later: no await, nothing unbounded, and
//! the reason still travels the one channel every other verdict uses.

use tokio::sync::mpsc::OwnedPermit;
use tokio::sync::mpsc::error::TrySendError;

use gsb_core::channel::Mailbox;
use gsb_core::conn::ConnIn;

/// Where this pump's verdict goes.
pub(crate) enum Verdict {
    /// The slot reserved at birth (every pump whose clock is on, in
    /// practice: its mailbox is fresh when the pumps start).
    Reserved(OwnedPermit<ConnIn>),
    /// The mailbox was already full at birth, so nothing was reserved:
    /// the post falls back to a `try_send`, and on a full mailbox to an
    /// awaited send after the close.
    Unreserved(Mailbox<ConnIn>),
    /// No verdict can ever be posted: the clock is off, or the actor was
    /// gone before the pump started.
    Never,
}

impl Verdict {
    /// Reserve the slot (synchronously: this runs before the writer task
    /// is spawned). A writer that never `judges` (a stream pump with its
    /// stall clock off) reserves nothing — it must not cost the reader a
    /// slot.
    pub(crate) fn reserve(in_tx: Mailbox<ConnIn>, judges: bool) -> Self {
        if !judges {
            return Self::Never;
        }
        match in_tx.try_reserve_owned() {
            Ok(slot) => Self::Reserved(slot),
            Err(TrySendError::Full(in_tx)) => Self::Unreserved(in_tx),
            Err(TrySendError::Closed(_)) => Self::Never,
        }
    }

    /// Post `msg` BEFORE the outbound channel is closed. Returns what is
    /// left to deliver after the close: only the unreserved fallback on a
    /// full mailbox leaves anything.
    pub(crate) fn post(self, msg: ConnIn) -> Option<(Mailbox<ConnIn>, ConnIn)> {
        match self {
            Self::Reserved(slot) => {
                slot.send(msg);
                None
            }
            Self::Unreserved(in_tx) => match in_tx.try_send(msg) {
                Err(TrySendError::Full(msg)) => Some((in_tx, msg)),
                Ok(()) | Err(TrySendError::Closed(_)) => None,
            },
            Self::Never => None,
        }
    }
}
