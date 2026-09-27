//! The room→registry close verb (BACKLOG E6): a room — or a shard —
//! that has ENDED a member's membership asks the registry to close that
//! member's connection too.
//!
//! ```text
//! room/shard tick ──try_send──▶ registry ──ConnIn::ServerClosed──▶ connection actor
//!  (queue, retried            (table settle,        (spawned send,      (verdict + best-effort
//!   next tick on Full)          one lookup)          never awaited)       ERROR 9, then close)
//! ```
//!
//! **Why through the registry.** The room holds no handle on the
//! connection actor — only on its outbound frame queue — and the
//! registry is the one actor that owns the connection table: the row
//! the close must settle (the affiliation, the cap slot, the sharded
//! member count) and the inbox the verdict travels on. It is the same
//! path every other registry-side verdict takes (`superseded`, the
//! birth caps): a spawned `ConnIn::ServerClosed` send, never awaited by
//! the registry, so a full connection inbox cannot stall the control
//! plane (the registry never awaits a room, and not a connection
//! either).
//!
//! **Who asks, today.** Only the input-idle ceiling under the opt-in
//! `RoomConfig::afk_action = Disconnect`. The verb is not exposed to game
//! logic in this round (see `docs/RECONNECT.md` §16).
//!
//! **Its keep-the-socket sibling** ([`LeaveRequest`], BACKLOG B40): under
//! the DEFAULT `afk_action = LeaveRoom` the ceiling ends the membership
//! and the connection stays open. The registry settles the row the same
//! way — the membership is over as if the client had sent
//! `LEAVE_ROOM_REQ` — and tells the connection it is out of the room
//! (`ConnIn::LeftRoom`, nothing on the wire), so a direct join works.

use tokio::sync::mpsc::error::TrySendError;

use crate::channel::Mailbox;
use crate::conn::ServerClose;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::registry::RegistryMsg;

/// A room's request to close one of its (former) members' connections:
/// [`RegistryMsg::CloseConn`].
///
/// Sent AFTER the room has ended the membership through its ordinary
/// disconnect path (`GameLogic::on_disconnect` ran; the entity was
/// despawned or parked), so the registry's job is only the table and the
/// transport — the entity's fate is already decided room-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseRequest {
    /// The connection to close.
    pub conn: ConnectionId,
    /// The room whose membership ended (the registry acts only on a row
    /// still affiliated with it — a stale request is a no-op).
    pub room: RoomId,
    /// The entity the membership held (the stale guard's second key, the
    /// one a leave carries too: a request can never close a LATER
    /// membership of the same connection).
    pub entity: EntityId,
    /// `true` = the disconnect policy PARKED the entity (`Detach::Hold`):
    /// its slot stays held, so the registry keeps the row as detached;
    /// `false` = it was despawned: the row's affiliation and slot go.
    pub parked: bool,
    /// The verdict the connection books (`server_closes{reason}`).
    pub cause: ServerClose,
    /// The human-readable reason: the `ERROR` code 9 message.
    pub reason: String,
}

/// A room's report that it ENDED a member's membership while the
/// member's connection stays open: [`RegistryMsg::LeaveConn`] (the
/// input-idle ceiling under the default `afk_action = LeaveRoom`, BACKLOG
/// B40). The keep-the-socket sibling of [`CloseRequest`]: same settlement
/// of the row, same stale guard, no verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveRequest {
    /// The connection whose membership ended (it stays open).
    pub conn: ConnectionId,
    /// The room whose membership ended (stale guard, first key).
    pub room: RoomId,
    /// The entity the membership held (stale guard, second key).
    pub entity: EntityId,
    /// `Some(key)` = the disconnect policy PARKED the entity and the room
    /// re-keyed the parked row to `key` ([`ConnectionId::park_key`]): the
    /// registry moves the membership's row there, where it is an ordinary
    /// detached row (slot held, resumable, released by the hold's end or
    /// a resume). `None` = despawned, or a park the room could not key
    /// (its key was taken, or the park already ended before this request
    /// left the room): the membership's slot goes back like a leave's.
    pub park: Option<ConnectionId>,
}

/// Hand every queued request to the registry's mailbox with a
/// synchronous `try_send` (a tick body never awaits).
///
/// The full-mailbox rule: a request the mailbox refuses as FULL stays in
/// `queue`, in order, and is retried on the next tick — it is never
/// dropped. A dropped request would leave open the very socket the
/// deployment asked to close (the whole point of the opt-in), and a
/// counted drop recovers nothing. The queue is bounded by membership: a
/// member's membership ends once, so it asks once, and a saturated
/// registry drains it as soon as it catches up. A CLOSED mailbox (the
/// registry is gone — the process is coming down) drops the request:
/// there is no table left and the teardown cascade closes every
/// connection anyway. The detach-despawn reports (`despawn_reports`)
/// follow the same two rules.
pub(crate) fn flush_close_requests(registry: &Mailbox<RegistryMsg>, queue: &mut Vec<CloseRequest>) {
    if queue.is_empty() {
        return;
    }
    for req in std::mem::take(queue) {
        if let Err(TrySendError::Full(RegistryMsg::CloseConn(req))) =
            registry.try_send(RegistryMsg::CloseConn(req))
        {
            queue.push(req);
        }
    }
}

/// [`flush_close_requests`] for [`LeaveRequest`]s: the same rules (Full
/// keeps the request, in order, for the next tick; Closed drops it).
pub(crate) fn flush_leave_requests(registry: &Mailbox<RegistryMsg>, queue: &mut Vec<LeaveRequest>) {
    if queue.is_empty() {
        return;
    }
    for req in std::mem::take(queue) {
        if let Err(TrySendError::Full(RegistryMsg::LeaveConn(req))) =
            registry.try_send(RegistryMsg::LeaveConn(req))
        {
            queue.push(req);
        }
    }
}

#[cfg(test)]
mod tests;
