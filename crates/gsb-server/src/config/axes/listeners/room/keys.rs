//! The room-level keys' own values, refused before anything binds —
//! flat or in a `[rooms.<id>]` — where a value has no meaning as a room;
//! and the input rate limit's two keys, read as one limit (BACKLOG E1).

use gsb_core::room::InputRate;

use crate::config::{Config, ServerError};

/// Why `conn_action = 0` is refused (BACKLOG F21).
const ZERO_ACTIONS: &str = "= 0: a connection's action channel must hold at least one \
     action (a zero-capacity channel has no meaning; omit the key for the \
     default 256)";

/// Why `input_burst` is refused without a rate in its table.
const BURST_ALONE: &str = "is written without `input_rate_hz` in the same table: a burst \
     needs `input_rate_hz` beside it (a burst layered over an inherited \
     rate would change a limit the table does not name)";

/// Why `input_burst` is refused next to `input_rate_hz = 0`.
const BURST_OFF: &str = "is written for a limit that is off (`input_rate_hz = 0`): remove \
     it, or give the rate";

/// Why `input_burst = 0` is refused.
const BURST_ZERO: &str = "= 0: a bucket of zero admits nothing (omit it for one second's \
     worth, `input_rate_hz`; `input_rate_hz = 0` turns the limit off)";

/// What one table's `input_rate_hz` / `input_burst` say: `None` = not
/// written (the layer below decides), `Some(None)` = off, `Some(Some(r))`
/// = the limit `r` — or why the pair cannot be read (refused at startup
/// by [`Config::check_room_keys`]).
///
/// The pair travels together: a table that writes a limit writes its
/// whole limit, so layering never mixes one table's rate with another's
/// burst.
pub(super) fn input_limit(
    rate: Option<u32>,
    burst: Option<u32>,
) -> Result<Option<Option<InputRate>>, &'static str> {
    match (rate, burst) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(BURST_ALONE),
        (Some(0), None) => Ok(Some(None)),
        (Some(0), Some(_)) => Err(BURST_OFF),
        (Some(_), Some(0)) => Err(BURST_ZERO),
        (Some(r), b) => Ok(Some(InputRate::new(r, b.unwrap_or(r)))),
    }
}

impl Config {
    /// Refuse a room-level key whose value no room can run with, naming
    /// the key and where it was written:
    ///
    /// - `conn_action = 0` (F21): it reached `mpsc::channel(0)` and
    ///   panicked the room actor at its first join. The core now reads
    ///   `0` as one slot (`RoomConfig::action_capacity`), so a hand-built
    ///   config cannot panic a room; a config FILE that says `0` meant
    ///   something else, and is told so.
    /// - `input_burst` without `input_rate_hz > 0` in the same table, or
    ///   `input_burst = 0` (E1): see [`input_limit`].
    pub(crate) fn check_room_keys(&self) -> Result<(), ServerError> {
        if self.conn_action == 0 {
            return Err(refused(None, "conn_action", ZERO_ACTIONS));
        }
        input_limit(self.input_rate_hz, self.input_burst)
            .map_err(|why| refused(None, "input_burst", why))?;
        for (&id, o) in &self.rooms {
            if o.conn_action == Some(0) {
                return Err(refused(Some(id), "conn_action", ZERO_ACTIONS));
            }
            input_limit(o.input_rate_hz, o.input_burst)
                .map_err(|why| refused(Some(id), "input_burst", why))?;
        }
        Ok(())
    }

    /// The flat keys' input limit over the game's default: written (on
    /// or off) wins, omitted keeps `game`.
    pub(super) fn input_rate_over(&self, game: Option<InputRate>) -> Option<InputRate> {
        match input_limit(self.input_rate_hz, self.input_burst) {
            Ok(Some(limit)) => limit,
            // Not written — or unreadable, which startup refuses.
            Ok(None) | Err(_) => game,
        }
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
