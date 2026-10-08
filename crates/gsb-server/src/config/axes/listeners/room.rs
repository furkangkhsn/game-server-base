//! The room every hosted game gets: the config's room-level keys, mapped
//! onto the core's `RoomConfig` in one place — for the rooms the server
//! pre-creates at startup AND for the rooms the admin surface opens at
//! runtime (`POST /rooms/open`), whatever the game (a sharded game's
//! shards included: the registry hands each shard this config) — and,
//! for an id with a `[rooms.<id>]` section, that room's own values laid
//! over it (BACKLOG B18; [`RoomOverride`]).

use std::collections::BTreeMap;

use gsb_core::id::RoomId;
use gsb_core::room::{AfkAction, InputRate, RoomConfig};
use tracing::info;

use crate::config::{Config, ServerError};

mod keys;
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

/// What the hosted game sets as the default of its rooms, under the
/// config file's keys: the input rate limit (`GameModule::input_rate`,
/// E1) and the input-idle ceiling's action (`GameModule::afk_action`,
/// E6). `Default` = no game: no limit, `leave_room`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GameDefaults {
    pub(crate) input_rate: Option<InputRate>,
    pub(crate) afk_action: AfkAction,
}

/// What a PUSH frame adds around an event body (the opcode, the `Push`
/// fields' tags and varints), with room to spare: `max_event_bytes` may
/// not come closer than this to the transports' frame bound.
const PUSH_HEADER_ROOM: usize = 64;

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
    /// core's defaults, and its per-room overrides — with no game default
    /// under the input limit (the file's view; see
    /// [`Self::room_template_for`]).
    pub(crate) fn room_template(&self) -> RoomTemplate {
        self.room_template_for(GameDefaults::default())
    }

    /// The room template of this config hosting a game whose room
    /// defaults are `game`: each layered low to high — the core's
    /// default, the game's value, the flat key, then `[rooms.<id>]`.
    /// The input rate limit (`GameModule::input_rate`): the number is the
    /// game's, the operator overrides it (`0` = off). The idle ceiling's
    /// action (`GameModule::afk_action`): a written `afk_action` wins.
    pub(crate) fn room_template_for(&self, game: GameDefaults) -> RoomTemplate {
        let base = RoomConfig {
            tick_hz: self.tick_hz,
            control_capacity: self.room_control,
            action_capacity: self.conn_action,
            max_snapshot_bytes: self.max_snapshot_bytes,
            keepalive_hz: self.keepalive_hz,
            max_players: self.max_players.map(|n| n as usize),
            max_idle_input_secs: self.max_idle_input_secs,
            max_detach_hold: self.max_detach_hold,
            afk_action: self.afk_action.unwrap_or(game.afk_action),
            input_rate: self.input_rate_over(game.input_rate),
            call_share: self.call_share,
            call_completions: self.call_completions,
            call_timeout: (self.call_timeout_secs > 0.0)
                .then(|| std::time::Duration::from_secs_f64(self.call_timeout_secs)),
            event_capacity: self.event_capacity,
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
    /// own. The FILE's view: a game's room defaults (its input rate
    /// limit, `GameModule::input_rate`, and its idle ceiling's action,
    /// `GameModule::afk_action`) are not in it — a running server's
    /// room, with them, is [`ServerHandle::room_config`].
    ///
    /// [`ServerHandle::room_config`]: crate::ServerHandle::room_config
    ///
    /// [`ServerHandle::open_room`]: crate::ServerHandle::open_room
    pub fn room_config(&self, id: u64) -> RoomConfig {
        self.room_template().room(id)
    }

    /// Refuse, before anything binds, call-table knobs no room can run
    /// with (BACKLOG B155): a zero `call_share`, a `call_completions`
    /// below 2 × `call_share` (SERVICES.md §19 S7), a `call_timeout_secs`
    /// that is negative or not finite.
    pub(crate) fn check_call_table(&self) -> Result<(), ServerError> {
        if self.call_share == 0 {
            return Err(ServerError::BadCallTable(
                "`call_share` must be >= 1 (0 would refuse every call)".into(),
            ));
        }
        let floor = 2 * self.call_share as usize;
        if self.call_completions < floor {
            return Err(ServerError::BadCallTable(format!(
                "`call_completions` {} is below 2 x `call_share` ({floor}): a room's \
                 answers could not all come back (SERVICES.md S7)",
                self.call_completions
            )));
        }
        if !self.call_timeout_secs.is_finite() || self.call_timeout_secs < 0.0 {
            return Err(ServerError::BadCallTable(format!(
                "`call_timeout_secs` {} must be finite and >= 0 (0 = off)",
                self.call_timeout_secs
            )));
        }
        Ok(())
    }

    /// Refuse, before anything binds, event knobs nothing can run with
    /// (`docs/EVENTS.md` §12, §20 E3): a zero `event_capacity` (a room
    /// that refuses every event), a zero `max_event_bytes`, or one a PUSH
    /// frame could not carry within the transports' frame bound.
    pub(crate) fn check_events(&self) -> Result<(), ServerError> {
        if self.event_capacity == 0 {
            return Err(ServerError::BadEvents(
                "`event_capacity` must be >= 1 (0 would refuse every event)".into(),
            ));
        }
        let ceiling = gsb_net::tcp::DEFAULT_MAX_FRAME_BYTES - PUSH_HEADER_ROOM;
        if self.max_event_bytes == 0 || self.max_event_bytes > ceiling {
            return Err(ServerError::BadEvents(format!(
                "`max_event_bytes` {} must be 1..={ceiling} (a PUSH frame carries the body \
                 plus a small header within the transports' frame bound)",
                self.max_event_bytes
            )));
        }
        Ok(())
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
