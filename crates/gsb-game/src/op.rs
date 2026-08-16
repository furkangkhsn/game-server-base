//! Game-band opcodes (>= [`gsb_protocol::op::GAME_BAND_START`]).

/// Client → server: "move my entity toward (x, y)".
pub const MOVE_TO: u16 = 1000;

/// Server → client: the complete, self-contained snapshot of the room's
/// snapshot group (membership is expressed by presence in the snapshot;
/// there are no separate spawn/remove events).
pub const WORLD_SNAPSHOT: u16 = 1003;

/// Reserved for per-connection private frames
/// ([`gsb_core::room::RoomLogic::private`]); unused by the demo game.
pub const PRIVATE: u16 = 1004;
