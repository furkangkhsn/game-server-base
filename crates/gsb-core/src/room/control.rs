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
    /// The entity lives on, parked. `grace = Some(d)` → the hold lasts
    /// `d`, then ends unless [`GameLogic::may_release`] vetoes (a logout
    /// timer that waits out a fight); `None` → only `may_release` ends it
    /// (combat-held). A veto can extend either hold at most until
    /// [`RoomConfig::max_detach_hold`] after the disconnect (the ceiling
    /// that makes a harass-lock impossible to extend forever). What
    /// happens at the end: the player returns first (resume) or `to`
    /// runs.
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
    /// [`Self::Detach`] of a connection a SERVER verdict closed (BACKLOG
    /// F28): the policy is asked with
    /// [`DisconnectCause::ConnectionClosedBy`]`(verdict)` — the idle
    /// timeout, write stall, dead rUDP band, violation budget, a newer
    /// session's takeover, another membership's kick, … that the
    /// connection booked. A separate variant so that a hand-built
    /// `Detach` (the client's own end, as before) keeps its shape; the
    /// registry sends `Detach` when the client ended the session.
    DetachBy {
        conn: ConnectionId,
        entity: EntityId,
        identity: String,
        verdict: crate::conn::ServerClose,
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
///
/// The lifetime is the actor's two lends: its input-idle clock
/// ([`Self::idle`]), so a logic hook can ask "how long since this player
/// last acted?", and the tick's kick queue ([`Self::kicks`]), so it can
/// ask that a player be kicked — both without an await, without a
/// per-player task and without a new method on the logic surface.
#[derive(Debug, Clone, Copy)]
pub struct TickCtx<'a> {
    pub room: RoomId,
    /// Global tick index (from the ticker; all rooms share one clock).
    pub tick: u64,
    /// Time since the previous step (covers any ticks missed in between).
    pub dt: Duration,
    /// Per-player input idleness — the AFK signal (see [`IdleView`] for
    /// what counts as input). A hand-built context defaults to the EMPTY
    /// view, whose every answer is `None`.
    pub idle: IdleView<'a>,
    /// The kick verb (BACKLOG E8; [`Self::kick`] is the shorthand). A
    /// hand-built context defaults to the INERT handle, which keeps
    /// nothing; a test that wants to read its logic's kicks lends one
    /// from its own [`KickQueue`].
    pub kicks: Kicks<'a>,
}

impl TickCtx<'_> {
    /// Time since `player`'s last action-bearing frame. `None` means the
    /// player has no input clock here: not a member, parked (detached),
    /// or bot-fed. Shorthand for [`IdleView::since_input`].
    pub fn since_input(&self, player: PlayerId) -> Option<Duration> {
        self.idle.since_input(player)
    }

    /// Kick `player` from the server: its membership ends through the
    /// ordinary disconnect path — [`GameLogic::on_disconnect`] is called
    /// with its resume identity and the returned [`Detach`] decides the
    /// entity's fate (park / AI handover / despawn) — and then its
    /// connection is closed: a best-effort `ERROR` code 9 whose message
    /// is `kicked: <reason>` (`reason` cut to
    /// [`KICK_REASON_MAX_BYTES`] on a `char` boundary; `kicked` alone
    /// when empty), then the close, counted as
    /// `server_closes{reason="kicked"}` (`docs/RECONNECT.md` §16.3).
    ///
    /// **Only queued.** Nothing happens inside the hook that asks. The
    /// actor applies the tick's kicks once the hooks that could ask have
    /// returned: those asked from `ingest`, `handle_request` or `update`
    /// (and a shard's seam hooks) right after SYSTEMS — so the kicked
    /// member gets no snapshot of this tick and, on a shard, never
    /// migrates this tick — and those asked from the broadcast-phase
    /// hooks (`snapshot`, `keepalive`, a shard's `team_exchange`) at the
    /// end of the tick. No CONTROL phase runs in between, so a kick
    /// judges the membership the asking hook saw. The close request
    /// leaves at the next tick's phase 0d (the input-idle close's queue
    /// and rules: a full registry mailbox keeps it for the tick after).
    ///
    /// **A no-op** (not counted) for a player that is not a live member
    /// of THIS room or shard when the kick is applied: unknown, already
    /// gone, parked, bot-fed, or — on a shard — migrated away before the
    /// hook asked. A second kick of the same player in one tick finds it
    /// gone (or parked) and is a no-op too: one close, the first reason.
    pub fn kick(&self, player: PlayerId, reason: impl Into<String>) {
        self.kicks.kick(player, reason);
    }
}
