//! The room-level keys' own values, refused before anything binds —
//! flat or in a `[rooms.<id>]` — where a value has no meaning as a room.

use crate::config::{Config, ServerError};

/// Why `conn_action = 0` is refused (BACKLOG F21).
const ZERO_ACTIONS: &str = "= 0: a connection's action channel must hold at least one \
     action (a zero-capacity channel has no meaning; omit the key for the \
     default 256)";

impl Config {
    /// Refuse a room-level key whose value no room can run with, naming
    /// the key and where it was written:
    ///
    /// - `conn_action = 0` (F21): it reached `mpsc::channel(0)` and
    ///   panicked the room actor at its first join. The core now reads
    ///   `0` as one slot (`RoomConfig::action_capacity`), so a hand-built
    ///   config cannot panic a room; a config FILE that says `0` meant
    ///   something else, and is told so.
    pub(crate) fn check_room_keys(&self) -> Result<(), ServerError> {
        if self.conn_action == 0 {
            return Err(refused(None, "conn_action", ZERO_ACTIONS));
        }
        for (&id, o) in &self.rooms {
            if o.conn_action == Some(0) {
                return Err(refused(Some(id), "conn_action", ZERO_ACTIONS));
            }
        }
        Ok(())
    }
}

/// The refusal of `key`, written flat (`room = None`) or in
/// `[rooms.<room>]`.
fn refused(room: Option<u64>, key: &str, reason: &'static str) -> ServerError {
    let at = match room {
        None => format!("`{key}`"),
        Some(id) => format!("`[rooms.{id}]` `{key}`"),
    };
    ServerError::RoomKey { at, reason }
}

#[cfg(test)]
mod tests;
