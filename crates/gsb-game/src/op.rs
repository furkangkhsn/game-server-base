//! Game-band opcodes (>= [`gsb_protocol::op::GAME_BAND_START`]).

/// Client → server: "move my entity toward (x, y)".
pub const MOVE_TO: u16 = 1000;

/// Server → client: the complete, self-contained snapshot of the room's
/// snapshot group (membership is expressed by presence in the snapshot;
/// there are no separate spawn/remove events).
pub const WORLD_SNAPSHOT: u16 = 1003;

/// Reserved for per-connection private frames
/// ([`gsb_core::room::RoomLogic::private`]); the demo game uses it for
/// input acks, one-shot full views, and RPC responses
/// (`Private.responses`).
pub const PRIVATE: u16 = 1004;

/// RPC inner op (carried inside `gsb_protocol::op::base::RPC_REQ`):
/// room-local request — answered in the same tick (see `game.proto`).
pub const ABILITY: u16 = 1005;

/// RPC inner op: external-I/O request — delegated to the in-process
/// economy service (the reference adapter), answered on a later tick.
pub const ECONOMY: u16 = 1006;
