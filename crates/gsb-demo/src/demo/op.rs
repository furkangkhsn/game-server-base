//! Game-band opcodes (>= [`gsb_protocol::op::GAME_BAND_START`]).
//!
//! # Retired opcodes
//!
//! [`RETIRED`] lists opcodes this game once used and removed. An opcode
//! is the same kind of contract as a protobuf field number: recycling a
//! retired one with a different message silently misparses for anyone
//! still speaking the old protocol. The `.proto` side expresses this
//! with `reserved`; the opcode space has no such keyword, so the list
//! plus [`crate::register`]'s test is the enforcement.
//!
//! This game has already paid for the lesson once: commit 2ac28d2 reused
//! 1003 (`ENTITY_STATE`) for `WORLD_SNAPSHOT` while retiring 1001 and
//! 1002 beside it.

/// Opcodes this game used and removed; never reuse them for a new
/// message.
///
/// - `1001` — `ENTITY_SPAWNED` ("an entity entered the room"), and
/// - `1002` — `ENTITY_REMOVED` ("an entity left the room"),
///
/// both introduced in 88b0544 and deleted in 2ac28d2, when membership
/// became "presence in the group snapshot" and the two event frames had
/// nothing left to say. See `proto/game.proto`'s header for the message
/// definitions that went with them.
pub const RETIRED: [u16; 2] = [1001, 1002];

/// Client → server: "move my entity toward (x, y)".
pub const MOVE_TO: u16 = 1000;

/// Server → client: the complete, self-contained snapshot of the room's
/// snapshot group (membership is expressed by presence in the snapshot;
/// there are no separate spawn/remove events).
pub const WORLD_SNAPSHOT: u16 = 1003;

/// Reserved for per-connection private frames
/// ([`gsb_core::room::GameLogic::private`]); the demo game uses it for
/// input acks, one-shot full views, and RPC responses
/// (`Private.responses`).
pub const PRIVATE: u16 = 1004;

/// RPC inner op (carried inside `gsb_protocol::op::base::RPC_REQ`):
/// room-local request — answered in the same tick (see `game.proto`).
pub const ABILITY: u16 = 1005;

/// RPC inner op: external-I/O request — delegated to the in-process
/// economy service (the reference adapter), answered on a later tick.
pub const ECONOMY: u16 = 1006;
