//! Game-band opcodes (>= [`gsb_protocol::op::GAME_BAND_START`]).

/// Client → server: "move my entity toward (x, y)".
pub const MOVE_TO: u16 = 1000;

/// Server → client: an entity entered the room.
pub const ENTITY_SPAWNED: u16 = 1001;

/// Server → client: an entity left the room / was removed.
pub const ENTITY_REMOVED: u16 = 1002;

/// Server → client: full snapshot of an entity (sent when its version
/// changed since the last snapshot).
pub const ENTITY_STATE: u16 = 1003;
