//! The room every hosted game gets: the config's room-level keys, mapped
//! onto the core's `RoomConfig` in one place — for the rooms the server
//! pre-creates at startup AND for the rooms the admin surface opens at
//! runtime (`POST /rooms/open`), whatever the game (a sharded game's
//! shards included: the registry hands each shard this config) — and,
//! for an id with a `[rooms.<id>]` section, that room's own values laid
//! over it (BACKLOG B18; [`RoomOverride`]).

use std::collections::BTreeMap;

use gsb_core::id::RoomId;
use gsb_core::room::RoomConfig;
use tracing::info;

use crate::config::{Config, ServerError};

mod overrides;
pub use overrides::RoomOverride;
pub(in crate::config) use overrides::deserialize as deserialize_overrides;

/// The room configuration this server uses, id aside: the one source
/// every creation path builds its rooms from. Only [`Config`] makes one,
/// so no path can fall back to the core's defaults behind the operator's
/// back (BACKLOG F8: the admin surface once built `RoomConfig::default()`
/// plus a rate and ignored every room-level key).
///
/// The per-room overrides ride along, so the ONE layering point below
/// serves every path: an id's `[rooms.<id>]` applies wherever that id is
/// built — a boot room, an admin open, `Config::room_config`.
#[derive(Debug, Clone)]
pub(crate) struct RoomTemplate {
    /// The server's room: the flat room-level keys over the core's
    /// defaults.
    base: RoomConfig,
    /// `[rooms.<id>]`, by id.
    overrides: BTreeMap<u64, RoomOverride>,
}

impl RoomTemplate {
    /// Room `id` of this server: the server's room, with the id's
    /// overrides (if any) laid over it.
    pub(crate) fn room(&self, id: u64) -> RoomConfig {
        let mut room = RoomConfig {
            id: RoomId(id),
            ..self.base.clone()
        };
        if let Some(o) = self.overrides.get(&id) {
            o.apply(&mut room);
        }
        room
    }
}

impl Config {
    /// The room template of this config: its room-level keys over the
    /// core's defaults, and its per-room overrides.
    pub(crate) fn room_template(&self) -> RoomTemplate {
        let base = RoomConfig {
            tick_hz: self.tick_hz,
            control_capacity: self.room_control,
            action_capacity: self.conn_action,
            max_snapshot_bytes: self.max_snapshot_bytes,
            keepalive_hz: self.keepalive_hz,
            max_players: self.max_players.map(|n| n as usize),
            max_idle_input_secs: self.max_idle_input_secs,
            max_detach_hold: self.max_detach_hold,
            ..RoomConfig::default()
        };
        RoomTemplate {
            base,
            overrides: self.rooms.clone(),
        }
    }

    /// Room `id`'s configuration: the room-level keys of this config over
    /// the core's defaults, and `[rooms.<id>]` over those — the room every
    /// creation path of the server builds, and what
    /// [`ServerHandle::open_room`] takes to open a room like the server's
    /// own.
    ///
    /// [`ServerHandle::open_room`]: crate::ServerHandle::open_room
    pub fn room_config(&self, id: u64) -> RoomConfig {
        self.room_template().room(id)
    }

    /// Refuse, before anything binds, a `[rooms.<id>]` whose room the
    /// registry would refuse (`RoomConfig::step_divisor`: its `tick_hz`
    /// must divide the global one, its `keepalive_hz` must not exceed its
    /// `tick_hz`) — at boot such a room would only log a warning, at
    /// runtime answer 400. An override for an id past `room_count` is
    /// valid (the room an admin open of that id builds) and logged as
    /// such, so a mistyped id is visible at startup.
    pub(crate) fn check_room_overrides(&self) -> Result<(), ServerError> {
        if self.rooms.is_empty() {
            return Ok(());
        }
        // A global rate without a period is its own startup error; say
        // that rather than blame every room for not dividing it.
        if !(self.tick_hz.is_finite() && self.tick_hz > 0.0) {
            return Err(ServerError::BadTickRate(self.tick_hz));
        }
        let template = self.room_template();
        for &id in self.rooms.keys() {
            template
                .room(id)
                .step_divisor(self.tick_hz)
                .map_err(|source| ServerError::RoomOverride { id, source })?;
            if id > self.room_count {
                info!(
                    room = id,
                    room_count = self.room_count,
                    "`[rooms.{id}]` is not a boot room: it applies when the id is \
                     opened at runtime (POST /rooms/open, ServerHandle::open_room)"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
