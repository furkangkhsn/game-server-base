//! The room every hosted game gets: the config's room-level keys, mapped
//! onto the core's `RoomConfig` in one place — for the rooms the server
//! pre-creates at startup AND for the rooms the admin surface opens at
//! runtime (`POST /rooms/open`), whatever the game (a sharded game's
//! shards included: the registry hands each shard this config).

use gsb_core::id::RoomId;
use gsb_core::room::RoomConfig;

use crate::config::Config;

/// The room configuration this server uses, id aside: the one source
/// every creation path builds its rooms from. Only [`Config`] makes one,
/// so no path can fall back to the core's defaults behind the operator's
/// back (BACKLOG F8: the admin surface once built `RoomConfig::default()`
/// plus a rate and ignored every room-level key).
#[derive(Debug, Clone)]
pub(crate) struct RoomTemplate(RoomConfig);

impl RoomTemplate {
    /// Room `id` of this server.
    pub(crate) fn room(&self, id: u64) -> RoomConfig {
        RoomConfig {
            id: RoomId(id),
            ..self.0.clone()
        }
    }
}

impl Config {
    /// The room template of this config: its room-level keys over the
    /// core's defaults.
    pub(crate) fn room_template(&self) -> RoomTemplate {
        RoomTemplate(RoomConfig {
            tick_hz: self.tick_hz,
            control_capacity: self.room_control,
            action_capacity: self.conn_action,
            max_snapshot_bytes: self.max_snapshot_bytes,
            keepalive_hz: self.keepalive_hz,
            max_players: self.max_players.map(|n| n as usize),
            max_idle_input_secs: self.max_idle_input_secs,
            max_detach_hold: self.max_detach_hold,
            ..RoomConfig::default()
        })
    }

    /// Room `id`'s configuration: the room-level keys of this config over
    /// the core's defaults — the room every creation path of the server
    /// builds, and what [`ServerHandle::open_room`] takes to open a room
    /// like the server's own.
    ///
    /// [`ServerHandle::open_room`]: crate::ServerHandle::open_room
    pub fn room_config(&self, id: u64) -> RoomConfig {
        self.room_template().room(id)
    }
}

#[cfg(test)]
mod tests;
