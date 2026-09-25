//! The room every hosted game gets: the config's room-level keys, mapped
//! onto the core's `RoomConfig` in one place (the rooms the server
//! pre-creates at startup, whatever the game — a sharded game's shards
//! included: the registry hands each shard this config).

use gsb_core::id::RoomId;
use gsb_core::room::RoomConfig;

use crate::config::Config;

impl Config {
    /// Room `id`'s configuration: the room-level keys of this config over
    /// the core's defaults.
    pub fn room_config(&self, id: u64) -> RoomConfig {
        RoomConfig {
            id: RoomId(id),
            tick_hz: self.tick_hz,
            control_capacity: self.room_control,
            action_capacity: self.conn_action,
            max_snapshot_bytes: self.max_snapshot_bytes,
            keepalive_hz: self.keepalive_hz,
            max_players: self.max_players.map(|n| n as usize),
            max_idle_input_secs: self.max_idle_input_secs,
            max_detach_hold: self.max_detach_hold,
            ..RoomConfig::default()
        }
    }
}

#[cfg(test)]
mod tests;
