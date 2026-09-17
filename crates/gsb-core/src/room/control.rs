//! The control plane's vocabulary: what the registry can ask a room
//! to do, what the logic answers about a disconnect, and the per-tick
//! context every logic hook receives.

use crate::channel::{FrameBatch, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, PlayerId, RoomId};
use crate::room::*;
use std::fmt::Debug;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// What a room's logic wants to happen to a disconnected player's entity
/// (the detach policy — `docs/RECONNECT.md` §3: *transport death is a
/// fact; what happens to the entity is a game rule*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detach {
    /// The old behavior: despawn the entity right away (lobby, chat).
    /// This is the trait method's default, so every pre-reconnect logic
    /// keeps byte-for-byte its old semantics without recompiling anything.
    Despawn,
    /// The entity lives on, parked. `grace = None` → only
    /// [`GameLogic::may_release`] ends the hold (combat-held);
    /// `Some(d)` → the hold ends at the latest `d` after the disconnect
    /// (the ceiling that makes a harassed lock impossible to extend
    /// forever). What happens at the end: the player returns first
    /// (resume) or `to` runs.
    Hold {
        grace: Option<Duration>,
        to: ExpireTo,
    },
}

/// Where a parked entity goes when its hold ends without a resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpireTo {
    /// Despawn (the slot is released; the identity may fresh-join later).
    Despawn,
    /// The same entity keeps playing, driven by the game's own input
    /// synthesis (`ExpireTo::AiHandover` — §9). The wire id is unchanged:
    /// handover is a behavior change, not an identity change. Tur A marks
    /// the connection `bot_fed` and keeps everything alive; the demo bot
    /// that synthesizes the input is Tur B's seam.
    AiHandover,
}

/// Outcome of the park-ledger lookup behind a resume attempt
/// (`docs/RECONNECT.md` §5/§7). The ledger lives in the logic (§4); this
/// is the one question the core asks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeFound {
    /// The identity is parked: `PlayerId` is the stable identity of the
    /// parked session's row (Faz 2 — the ledger rides the player state,
    /// §14.2, so it answers with the id that keys the core's own tables;
    /// the parked row is found by ONE lookup instead of a scan).
    Held(PlayerId),
    /// The identity WAS parked but the hold has ended (expired, consumed
    /// by an earlier resume, superseded). The resume mechanism rejects
    /// (counted as `resume_rejected_stale`) and — per the transparent
    /// fallback of §5, which produces no client-visible error — the join
    /// proceeds as an ordinary fresh join.
    Ended,
    /// Never parked (the default): transparent fresh-join fallback,
    /// indistinguishable from an ordinary join. This is what makes the
    /// grace TOCTOU self-resolving (§11): whichever of expire/resume
    /// lands first, both branches are valid.
    Never,
}

/// Control messages to a room. Low frequency; processed at the next tick
/// boundary (deterministic: joins and leaves take effect *on* a tick, never
/// mid-simulation; join/leave latency is at most one tick).
#[derive(Debug)]
pub enum RoomControl {
    /// A player joined: register its outbound channel, create its entity,
    /// and hand the connection actor the sender of the new per-connection
    /// action channel.
    ///
    /// The reply is a `Result`: the join protocol can *structurally* fail
    /// (a full room — [`crate::error::CoreError::RoomFull`] — rejects
    /// without creating the entity). The room's member count is the
    /// authority on capacity; the registry (which counts connections, not
    /// room members) never decides it.
    Join {
        conn: ConnectionId,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// A player left: remove its entity and channel.
    ///
    /// Carries the entity this leave refers to: a *stale* leave (its
    /// connection re-joined in the meantime) is ignored, so it can never
    /// despawn the new entity.
    Leave {
        conn: ConnectionId,
        entity: EntityId,
    },
    /// The connection's transport died: hand the entity's fate to the
    /// room policy (`GameLogic::on_disconnect` — `docs/RECONNECT.md` §3).
    /// This is what `ConnClosed` routes instead of the despawn-causing
    /// `Leave` it used to send: the registry never decides the policy,
    /// it only reports the fact.
    ///
    /// `identity` is the resume key (`ValidatedTicket.player`, or
    /// `Auth.name` on the local-auth path — demo-only there): the logic
    /// records it in its park ledger when it answers
    /// [`Detach::Hold`](crate::room::Detach::Hold).
    Detach {
        conn: ConnectionId,
        entity: EntityId,
        identity: String,
    },
    /// An identified join whose ledger may hold this identity: the
    /// implicit resume attempt (§14.3 — there is NO new wire opcode; a
    /// ticket-pinned connection's ordinary `JOIN_ROOM_REQ` IS the resume
    /// attempt). If the ledger holds the identity, the room swaps the
    /// channel halves onto the parked entity and replies with the SAME
    /// entity id; otherwise it processes an ordinary fresh join and
    /// replies identically (transparent fallback, §5) — the client
    /// cannot tell the two apart from the reply alone.
    Resume {
        conn: ConnectionId,
        /// The dispatcher-minted join epoch of the NEW session. Guard
        /// (§7): a resume whose epoch is not newer than the parked
        /// session's stamp is a delayed duplicate/replay — rejected with
        /// [`CoreError::ResumeStale`] so it can never double-bind or
        /// fresh-join a second entity for one identity. `0` (= "no
        /// epoch", hand-built calls) disables the guard; the ledger
        /// consumption stays exactly-once regardless, because the actor
        /// is single-threaded.
        epoch: u64,
        identity: String,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// Stop the room (drops the world).
    Shutdown,
}

/// Per-tick metadata handed to the game logic.
#[derive(Debug, Clone, Copy)]
pub struct TickCtx {
    pub room: RoomId,
    /// Global tick index (from the ticker; all rooms share one clock).
    pub tick: u64,
    /// Time since the previous step (covers any ticks missed in between).
    pub dt: Duration,
}
