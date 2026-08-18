//! Core error types.

/// Errors that can be reported across the control plane.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("room {0} not found")]
    RoomNotFound(u64),

    #[error("room {0} is full")]
    RoomFull(u64),

    #[error("connection is not in a room")]
    NotInRoom,

    #[error("room already exists: {0}")]
    RoomExists(u64),

    #[error("room tick rate {room} Hz does not divide the global tick rate {global} Hz")]
    TickRate { room: f64, global: f64 },

    #[error("keep-alive rate {keepalive} Hz exceeds the room tick rate {tick} Hz: keepalive_hz must be <= tick_hz (a keep-alive faster than the tick would clamp to every step, and clients would receive fewer keep-alives than configured)")]
    KeepaliveRate { keepalive: f64, tick: f64 },

    #[error("room shut down")]
    RoomGone,

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("io error: {0}")]
    Io(String),
}

impl From<gsb_protocol::ProtoError> for CoreError {
    fn from(e: gsb_protocol::ProtoError) -> Self {
        Self::Protocol(e.to_string())
    }
}
