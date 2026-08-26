//! The shard-to-neighbour seam. One trait, one in-process
//! implementation; a distributed link (docs/DISTRIBUTED.md) plugs in
//! here without the actor noticing.
use std::fmt::Debug;

use tokio::sync::mpsc;

use crate::channel::{Inbox, Mailbox};
use crate::shard::*;

/// the protocol to reason about, not a refactor. The seam does not
/// demultiplex — the CONTROL drain plus `handle_msg` stay the single
/// inbound authority.
pub(crate) type NeighborMsg<S, B> = ShardMsg<S, B>;

/// Why a best-effort [`ShardLink::send`] refused a message. The refused
/// value travels back INSIDE the error so a failed send loses nothing:
/// the migration path rolls the moved connection halves back out of the
/// exact message it tried to ship (a failed migration orphans nothing),
/// which is precisely what the raw `TrySendError` used to carry.
/// `Full` and `Closed` stay distinct because they answer to different
/// healing rules — full is transient backpressure (the §4 classes heal:
/// forced Full next tick, crossing re-collected next tick), closed means
/// this peer incarnation is gone (the room death watcher owns that
/// story), and some paths log only the transient case.
#[derive(Debug)]
pub(crate) enum LinkFull<M> {
    /// The link's bounded queue was full — the transient drop case.
    Full { msg: M },
    /// Nothing will ever dequeue from this link again (the peer's receive
    /// end is gone).
    Closed { msg: M },
}

impl<M> LinkFull<M> {
    /// Take the refused message back out: the rollback path reads its
    /// payload to restore exactly what the failed send would have
    /// consumed.
    pub(crate) fn into_msg(self) -> M {
        match self {
            LinkFull::Full { msg } | LinkFull::Closed { msg } => msg,
        }
    }
}

/// The shard↔neighbor communication contract (`docs/DISTRIBUTED.md`
/// §3): unifies in-process channels with future UDS/TCP links behind one
/// object-safe seam, so a distributed link can drop exactly where the
/// in-process one drops and every existing recovery path keeps working
/// unchanged. Send is BEST-EFFORT by design — delivery classes and
/// healing rules live in the message semantics (§4), not in the link.
/// Object-safe on purpose: the future Ipc/Net links will be separate
/// types held as trait objects beside today's.
/// Which border-exchange packaging a link's pair runs (ROADMAP Faz C):
/// derived from the LINK CLASS, never from operator config.
///
/// - `AlwaysFull` — same-process neighbors: bytes cross by move (free),
///   CPU is the scarce local resource, and full replacement skips the
///   per-tick diff entirely (measured: full 1–5 µs vs delta 48–227 µs
///   phase-5 — CROSS-SHARD §7 A/B).
/// - `Delta` — future Ipc/Net links: bytes are transport currency there,
///   so the 60% reduction pays for its diffing CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExchangeMode {
    AlwaysFull,
    /// Constructed only by tests TODAY (production InProc links are
    /// AlwaysFull); Ipc/Net links will construct it when they land
    /// (DISTRIBUTED §4b) — hence the allow instead of deletion.
    #[cfg_attr(not(test), allow(dead_code))]
    Delta,
}

pub(crate) trait ShardLink<S, B>: Send {
    /// En-queue one message for the peer, best-effort: a full or dead
    /// link refuses and hands the message BACK ([`LinkFull`]) — the
    /// caller's healing rules decide what that loss means. Never blocks,
    /// never awaits.
    fn send(&mut self, msg: NeighborMsg<S, B>)
    -> Result<(), LinkFull<NeighborMsg<S, B>>>;

    /// Take every queued inbound message, in send order (FIFO). The
    /// CONTROL phase drains through here; an empty queue yields an empty
    /// vec. Never blocks.
    fn drain(&mut self) -> Vec<NeighborMsg<S, B>>;

    /// The border-exchange packaging this link runs (ROADMAP Faz C):
    /// derived from the link CLASS, not operator config — same-process
    /// links declare AlwaysFull (bytes free over a move, local CPU
    /// scarce), future Ipc/Net links will declare Delta (bytes are the
    /// transport currency there). The actor consults this per neighbor
    /// when building phase-5 exchanges; mixed-mode neighborhoods are
    /// supported because receivers accept both variants at any time.
    fn exchange_mode(&self) -> ExchangeMode;
}

/// The in-process [`ShardLink`] (`docs/DISTRIBUTED.md` §3, "today"
/// column): wraps the existing bounded mpsc halves with ZERO transport
/// behavior of its own — `send` is `try_send` mapped onto [`LinkFull`],
/// `drain` is the plain `try_recv` loop — so capacity, FIFO order and
/// drop timing are the channel's, unchanged from the pre-seam wiring.
/// Either half may be absent: without a transmit end the link refuses
/// sends (`Closed` — it accepts nothing by construction); without a
/// receive end it delivers nothing. Both are total functions instead of
/// panics so the same type serves every wiring (per-neighbor outbound
/// slots, the actor's own inbound inbox, and the paired form tests use).
pub(crate) struct InProcLink<S, B> {
    /// The peer's mailbox (a clone of its shared per-shard inbox sender).
    pub(in crate::shard) tx: Option<Mailbox<ShardMsg<S, B>>>,
    /// This side's receive half. `None` on today's per-neighbor outbound
    /// slots: their inbound traffic lands in the shard's OWN shared
    /// inbox, whose link lives separately on the actor.
    pub(in crate::shard) rx: Option<Inbox<ShardMsg<S, B>>>,
    /// Same-process ⇒ AlwaysFull (see [`ExchangeMode`]). A field rather
    /// than a type-level fact only so tests can construct Delta-mode
    /// links against the identical struct.
    pub(in crate::shard) mode: ExchangeMode,
}

impl<S, B> InProcLink<S, B> {
    /// Wrap one outbound per-neighbor mailbox: the actor sends into the
    /// neighbor's shared inbox and never receives here.
    pub(crate) fn outbound(tx: Mailbox<ShardMsg<S, B>>) -> Self {
        Self {
            tx: Some(tx),
            rx: None,
            mode: ExchangeMode::AlwaysFull,
        }
    }

    /// Wrap the shard's own inbound inbox — the CONTROL drain's source,
    /// carrying registry control AND neighbor protocol messages on one
    /// bounded FIFO (registry.rs pass 1).
    pub(crate) fn inbound(rx: Inbox<ShardMsg<S, B>>) -> Self {
        Self {
            tx: None,
            rx: Some(rx),
            mode: ExchangeMode::AlwaysFull,
        }
    }
}

impl<S: Send, B: Send> ShardLink<S, B> for InProcLink<S, B> {
    fn exchange_mode(&self) -> ExchangeMode {
        self.mode
    }

    fn send(
        &mut self,
        msg: NeighborMsg<S, B>,
    ) -> Result<(), LinkFull<NeighborMsg<S, B>>> {
        match &self.tx {
            Some(tx) => tx.try_send(msg).map_err(|e| match e {
                mpsc::error::TrySendError::Full(msg) => LinkFull::Full { msg },
                mpsc::error::TrySendError::Closed(msg) => LinkFull::Closed { msg },
            }),
            // No transmit half: nothing was queued and nothing ever will
            // be — the permanent refusal, not backpressure.
            None => Err(LinkFull::Closed { msg }),
        }
    }

    fn drain(&mut self) -> Vec<NeighborMsg<S, B>> {
        let mut out = Vec::new();
        if let Some(rx) = &mut self.rx {
            while let Ok(m) = rx.try_recv() {
                out.push(m);
            }
        }
        out
    }
}
