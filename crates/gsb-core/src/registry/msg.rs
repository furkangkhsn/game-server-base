//! The registry's message vocabulary: every control-plane operation
//! the rest of the server can ask of it, and the reply channel each
//! one carries.

use std::fmt::Debug;

use tokio::sync::{mpsc, oneshot};

use crate::channel::{FrameBatch, Mailbox};
use crate::conn::ConnIn;
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::registry::*;
use crate::room::{Action, RoomConfig};

/// Messages addressed to the registry actor.
#[derive(Debug)]
pub enum RegistryMsg {
    /// Create and start a room.
    ///
    /// **Idempotent** (the control plane may resend the same request —
    /// retry-after-timeout is the normal pattern for a control plane):
    ///
    /// - room absent → created (the usual validation: tick rate,
    ///   keep-alive rate) and the reply is `Ok(Running { members: 0 })`;
    /// - room present with the IDENTICAL config → no-op, the reply is
    ///   `Ok(Running { members })` (one room, not two);
    /// - room present with a DIFFERENT config → `Err(RoomConflict)` —
    ///   a different spec is not a retry, it is a contradiction, and
    ///   silently accepting it would start a room the control plane did
    ///   not ask for.
    ///
    /// The comparison is over the WHOLE `RoomConfig` (it is `PartialEq`
    /// for this): a retry carries the same config by construction. The
    /// reply carries the room's status (see [`RoomStatus`]) so a create
    /// round trip is also a status query (one hop, not two).
    CreateRoom {
        config: RoomConfig,
        reply: oneshot::Sender<Result<RoomStatus, CoreError>>,
    },
    /// Shut down a room (its players' entities are dropped; connections are
    /// notified via [`ConnIn::RoomGone`]).
    ///
    /// Idempotent: destroying an absent room is a no-op and the reply is
    /// `Ok(Absent)` (a control plane that retries a destroy never sees an
    /// error for the second attempt). The reply is `Ok(Destroyed)` when
    /// the shutdown was issued — note the room actor processes the
    /// `Shutdown` on its next tick, so between the reply and the actual
    /// stop a status query may already report `Absent` (the table entry
    /// is removed when the destroy is ACCEPTED, which is the
    /// control-plane-relevant moment: no new joins can start).
    DestroyRoom {
        id: RoomId,
        reply: oneshot::Sender<RoomStatus>,
    },
    /// Query a room's status from the registry's table (no room round
    /// trip — the registry never awaits a room; the member count comes
    /// from the connection table, which the registry maintains from the
    /// join/leave reports of every room, sharded or single).
    RoomStatus {
        id: RoomId,
        reply: oneshot::Sender<RoomStatus>,
    },
    /// Spawn a player entity in a room and report the entity + the
    /// per-connection action channel the connection actor writes to.
    ///
    /// Non-blocking with respect to the room: the round-trip is dispatched
    /// to the connection's relationship task and the reply may arrive
    /// later (at the room's next tick boundary). A slow room can therefore
    /// never stall the registry.
    SpawnPlayer {
        conn: ConnectionId,
        room: RoomId,
        /// The connection's outbound channel, handed to the room for fan-out.
        out: mpsc::Sender<FrameBatch>,
        /// The resume key (`ValidatedTicket.player`, or `Auth.name` on the
        /// local-auth path — demo-only there). Empty = anonymous: an
        /// ordinary fresh join, no ledger lookup, no supersedence. A
        /// NON-empty identity makes this join the implicit resume attempt
        /// of §14.3 (the room falls back to a fresh join transparently
        /// when its ledger does not hold the identity).
        identity: String,
        reply: oneshot::Sender<Result<(EntityId, Mailbox<Action>), CoreError>>,
    },
    /// Remove a player from its room (voluntary leave). The connection
    /// stays registered (it may rejoin).
    DespawnPlayer { conn: ConnectionId },
    /// A connection registered itself so the registry can notify it later.
    ConnOpened {
        conn: ConnectionId,
        inbox: Mailbox<ConnIn>,
    },
    /// A connection went away for good; the registry removes its entry and
    /// makes sure its room-side entity is cleaned up.
    ConnClosed { conn: ConnectionId },
    /// A connection authenticated successfully (either auth path): it
    /// leaves the unauthenticated pool its [`Self::ConnOpened`] entered
    /// (docs/SECURITY.md §4). Sent by the connection actor itself, right
    /// where its state flips to authenticated. A notice for a connection
    /// the registry never recorded (rejected at a cap) or already removed
    /// is ignored: informational only. Detached/resumed sessions never
    /// re-enter the pool — an affiliation requires authentication, so a
    /// parked entry keeps its authenticated mark for its whole lifetime.
    Authed { conn: ConnectionId },
    /// Shut everything down: notify connections, destroy all rooms.
    Shutdown,

    // -- internal: reported by relationship dispatcher tasks ----------------
    /// A dispatched join completed: record the affiliation.
    ///
    /// `generation` is the room incarnation the join was dispatched
    /// against (stamped by the registry, echoed by the dispatcher): a
    /// mismatch with the current entry means the room died (or was
    /// replaced) between dispatch and settlement — the affiliation must
    /// NOT be recorded (see the handler; supervision).
    SpawnDone {
        conn: ConnectionId,
        room: RoomId,
        entity: EntityId,
        generation: u64,
    },
    /// A dispatched join was rejected by the room (e.g. the shard's
    /// wire-id range is exhausted): release the capacity reservation.
    /// The reservation is released only when `generation` still matches
    /// (a stale failure must not touch a rebuilt room's counters).
    SpawnFailed {
        conn: ConnectionId,
        room: RoomId,
        generation: u64,
    },
    /// A dispatched leave completed: clear the affiliation.
    LeaveDone { conn: ConnectionId, room: RoomId },
    /// A connection's transport died and its DETACH was delivered to the
    /// room (the dispatcher's `Close` now reports this instead of
    /// `LeaveDone`): the affiliation is KEPT but marked detached — the
    /// parked entity still holds its cap slot (§4), so the registry's
    /// member view must not drop either. The slot is released by exactly
    /// three events: a resume that re-affiliates the identity (SpawnDone
    /// cleanup), the room ending (destroy/death/`notify_room_gone`), and
    /// the room reporting the detach's end toward despawn
    /// ([`Self::DetachDespawned`]).
    DetachDone { conn: ConnectionId, room: RoomId },
    /// The detached session's entity is GONE: the room dropped it through
    /// the ordinary leave funnel, so release the detached row this
    /// registry has been holding the slot with.
    ///
    /// Sent from the two — and only two — places a detach reaches despawn
    /// (`docs/RECONNECT.md` §3, §4):
    ///
    /// 1. the DETACH itself, when the logic's `on_disconnect` answers
    ///    `Detach::Despawn` (it declines to park at all — the shape of
    ///    `disconnect_grace_secs = 0`, and of any per-player policy that
    ///    says this one is not worth holding). The room despawns in that
    ///    same control phase; nothing is ever parked;
    /// 2. the hold-expiry sweep, when a park that DID start runs out
    ///    toward `ExpireTo::Despawn`.
    ///
    /// WHY the room has to say this. Both the decision and the grace are
    /// room-side policy (the logic picks them per player, and a
    /// combat-held park has no deadline at all), so the room is the only
    /// actor that can know the detach ended; the registry cannot age
    /// these rows out on a timer of its own without being wrong about
    /// exactly the cases that matter. The registry has already marked the
    /// row `detached` and kept it by the time the policy answers — that
    /// mark is speculative, and this message is what resolves it.
    ///
    /// When the room told nobody, a detached row survived the entity it
    /// stood for — one leaked row plus one leaked `max_connections` slot
    /// per player who never came back, released only if the room itself
    /// ended. A persistent room never does.
    ///
    /// NAMING (this variant used to be `ParkExpired`). It was minted for
    /// sender 2 alone, and read as a lie on sender 1, where nothing was
    /// ever parked and nothing expired — the policy declined. The name
    /// now describes the registry-facing FACT ("this detached row's
    /// entity was despawned"), which is what both senders report and what
    /// the arm below acts on, rather than one of the two causes.
    ///
    /// Idempotent and self-guarding: the registry acts only on a row that
    /// is still detached AND still affiliated with `room`, so a report
    /// that races a resume (which already released the row and re-keyed
    /// the identity onto a live [`ConnectionId`]) is a silent no-op.
    /// Connection ids are minted monotonically and never reused, so the
    /// report can never land on a later session.
    ///
    /// Not sent for the AI-handover arm: that hold ends with the entity
    /// still ALIVE under a bot, still holding its slot, and still a valid
    /// resume target (`docs/RECONNECT.md` §9) — the row is doing its job
    /// there, not leaking.
    DetachDespawned { conn: ConnectionId, room: RoomId },
    /// A connection's dispatcher task exited; drop its slot.
    OpsClosed { conn: ConnectionId },
    /// Internal: reported by a room/shard death watcher (see
    /// [`Self::spawn_room_watcher`]) when the watched actor task has ended
    /// — by panic or by any normal exit (`DestroyRoom`, server
    /// `Shutdown`). The registry answers it with one table lookup, and the
    /// `generation` is what makes that lookup decisive: an entry of a
    /// DIFFERENT generation (destroyed and even re-created in the
    /// meantime) or no entry at all means the report is stale → silent
    /// no-op. Only a LIVE entry of the SAME incarnation is an *unexpected*
    /// death: reap it like a destroy (members notified, affiliations
    /// cleared) and, per [`RoomConfig::restart_on_panic`], rebuild.
    RoomDied {
        id: RoomId,
        /// The dead task's shard index (sharded rooms only).
        shard: Option<usize>,
        /// The incarnation this watcher was spawned for (the stale-watch
        /// guard; see [`Registry::install_room`]).
        generation: u64,
    },
}
