//! What the input-idle ceiling does to the member it expires
//! ([`RoomConfig::afk_action`](crate::room::RoomConfig::afk_action),
//! BACKLOG E6).

use std::time::Duration;

use crate::conn::ServerClose;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::registry::CloseRequest;

/// The input-idle ceiling's action — [`RoomConfig::afk_action`]: what
/// happens, beyond the ordinary disconnect policy, to a member whose last
/// action-bearing frame is older than
/// [`RoomConfig::max_idle_input_secs`].
///
/// Both actions first end the MEMBERSHIP the same way — the room runs
/// [`GameLogic::on_disconnect`](crate::room::GameLogic::on_disconnect)
/// and the game's `Detach` decides park / AI handover / despawn. They
/// differ in the TRANSPORT only. Parked and bot-fed rows are never on the
/// input clock, so neither action ever reaches them.
///
/// [`RoomConfig::afk_action`]: crate::room::RoomConfig::afk_action
/// [`RoomConfig::max_idle_input_secs`]: crate::room::RoomConfig::max_idle_input_secs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AfkAction {
    /// The default: the membership ends, the SOCKET STAYS OPEN — the
    /// session is like one that sent `LEAVE_ROOM_REQ`: authenticated and
    /// in no room (the room asks the registry to settle the row that way,
    /// [`crate::registry::RegistryMsg::LeaveConn`]), its game frames are
    /// answered `ERROR 6` and its next join goes straight through — the
    /// implicit resume attempt for a parked entity (`docs/RECONNECT.md`
    /// §14.3, §16). Nothing new on the wire.
    #[default]
    LeaveRoom,
    /// The membership ends as with [`Self::LeaveRoom`], AND the room asks
    /// the registry to close the connection
    /// ([`crate::registry::RegistryMsg::CloseConn`]): the client gets a
    /// best-effort `ERROR` code 9 (`input idle: …`) and then the close;
    /// the session is counted as `server_closes{reason="idle_input"}`
    /// ([`ServerClose::IdleInput`]). A parked entity stays parked — a
    /// reconnect with the same identity resumes it.
    Disconnect,
}

impl AfkAction {
    /// Every action, in the order the config spellings list them.
    pub const ALL: [AfkAction; 2] = [Self::LeaveRoom, Self::Disconnect];

    /// The config spelling (`afk_action = "…"`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::LeaveRoom => "leave_room",
            Self::Disconnect => "disconnect",
        }
    }

    /// The action a config spelling names, if any.
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.label() == word)
    }
}

/// The close request the ceiling sends for `conn` under
/// [`AfkAction::Disconnect`], after the disconnect policy ran: `parked`
/// says whether the policy parked the entity. The reason names the
/// ceiling so the client developer can tell it from a transport idle
/// timeout (same code 9).
pub(crate) fn idle_close(
    conn: ConnectionId,
    room: RoomId,
    entity: EntityId,
    parked: bool,
    limit: Duration,
) -> CloseRequest {
    CloseRequest {
        conn,
        room,
        entity,
        parked,
        cause: ServerClose::IdleInput,
        reason: format!(
            "input idle: no game input for {} s (the room's max_idle_input_secs; \
             afk_action = disconnect)",
            limit.as_secs()
        ),
    }
}

#[cfg(test)]
mod tests;
