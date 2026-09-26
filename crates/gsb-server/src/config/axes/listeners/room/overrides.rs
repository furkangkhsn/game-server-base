//! `[rooms.<id>]`: one room's own values for the ROOM-level keys (BACKLOG
//! B18), laid over the server's room template for that id alone
//! ([`RoomTemplate::room`](super::RoomTemplate::room)).
//!
//! The grammar is deliberately exactly as wide as the room template: the
//! same eight keys, spelled and read as their flat counterparts
//! (`max_players = 0` = no cap, `max_idle_input_secs = 0` = off,
//! `max_detach_hold_secs` in its three spellings). Any other key — a
//! typo, a server-wide key (`bind`, `max_connections`), a game setting
//! (those live in the game's own table) — refuses startup, listing the
//! keys a room override takes. Rejected shapes: `[[rooms]]` entries with
//! an `id` key — a duplicate id would need a check of its own, while a
//! TOML table cannot hold one key twice; a free-form table passed to the
//! game — room-level keys are the engine's, not a game's.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use gsb_core::room::RoomConfig;
use serde::de::{self, Deserializer, MapAccess, Visitor};

/// One room's overrides: every key omitted keeps the server's value.
/// See the flat [`Config`](crate::Config) keys of the same names for
/// what each one means.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomOverride {
    /// The room's rate; must divide the global `tick_hz` (checked at
    /// startup, the registry's rule).
    pub tick_hz: Option<f64>,
    /// The room's control channel capacity.
    pub room_control: Option<usize>,
    /// Each connection's action channel capacity in this room.
    pub conn_action: Option<usize>,
    /// The room's snapshot-size warning threshold, in bytes.
    pub max_snapshot_bytes: Option<usize>,
    /// The room's keep-alive rate, in Hz (`<= 0` disables); must not
    /// exceed the room's `tick_hz` (checked at startup).
    pub keepalive_hz: Option<f64>,
    /// The room's membership cap (`0` = no cap).
    pub max_players: Option<u32>,
    /// The room's input-idle ceiling, in seconds (`0` = off).
    pub max_idle_input_secs: Option<u64>,
    /// The room's detach-hold ceiling: outer `None` = not written; inner
    /// `None` = `"off"` (spelled `max_detach_hold_secs`).
    #[serde(
        rename = "max_detach_hold_secs",
        default,
        deserialize_with = "detach_hold"
    )]
    pub max_detach_hold: Option<Option<Duration>>,
}

impl RoomOverride {
    /// Lay the written keys over `room` (the server's room of this id),
    /// each read as its flat counterpart is.
    pub(super) fn apply(&self, room: &mut RoomConfig) {
        if let Some(hz) = self.tick_hz {
            room.tick_hz = hz;
        }
        if let Some(n) = self.room_control {
            room.control_capacity = n;
        }
        if let Some(n) = self.conn_action {
            room.action_capacity = n;
        }
        if let Some(n) = self.max_snapshot_bytes {
            room.max_snapshot_bytes = n;
        }
        if let Some(hz) = self.keepalive_hz {
            room.keepalive_hz = hz;
        }
        if let Some(n) = self.max_players {
            // 0 = no cap, as the flat key reads it.
            room.max_players = (n > 0).then_some(n as usize);
        }
        if let Some(secs) = self.max_idle_input_secs {
            room.max_idle_input_secs = Some(secs);
        }
        if let Some(hold) = self.max_detach_hold {
            room.max_detach_hold = hold;
        }
    }
}

/// `max_detach_hold_secs` as the flat key reads it, marked as written.
fn detach_hold<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<Duration>>, D::Error> {
    super::super::detach_hold::deserialize(d).map(Some)
}

/// Deserialize `[rooms]`: each key a room id written plainly (`7`, not
/// `07` or `lobby`), each value that room's overrides.
pub(in crate::config) fn deserialize<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<u64, RoomOverride>, D::Error> {
    d.deserialize_map(Rooms)
}

struct Rooms;

impl<'de> Visitor<'de> for Rooms {
    type Value = BTreeMap<u64, RoomOverride>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a table of room ids to room-level keys (`[rooms.7]`)")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut rooms = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            let id = room_id(&key).ok_or_else(|| {
                de::Error::custom(format!(
                    "`rooms.{key}`: a room override is keyed by its room id, a \
                     positive integer written plainly (`[rooms.7]`)"
                ))
            })?;
            rooms.insert(id, map.next_value()?);
        }
        Ok(rooms)
    }
}

/// The room id `key` spells: positive, canonical (so two spellings of
/// one id cannot both appear).
fn room_id(key: &str) -> Option<u64> {
    key.parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && id.to_string() == key)
}

#[cfg(test)]
mod tests;
