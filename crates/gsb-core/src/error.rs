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

    #[error(
        "room {0} already exists with a different configuration (an idempotent create must resend the identical request)"
    )]
    RoomConflict(u64),

    #[error("room tick rate {room} Hz does not divide the global tick rate {global} Hz")]
    TickRate { room: f64, global: f64 },

    #[error(
        "keep-alive rate {keepalive} Hz exceeds the room tick rate {tick} Hz: keepalive_hz must be <= tick_hz (a keep-alive faster than the tick would clamp to every step, and clients would receive fewer keep-alives than configured)"
    )]
    KeepaliveRate { keepalive: f64, tick: f64 },

    #[error(
        "invalid global tick rate {rate} Hz: must be finite and > 0 (the ticker derives its period as 1/rate, so a non-positive or non-finite rate has no period to run at)"
    )]
    InvalidTickRate { rate: f64 },

    #[error("room shut down")]
    RoomGone,

    /// The room id is RETIRED (§8): an ephemeral match ended, or a
    /// persistent room was decommissioned via `DestroyRoom`. The client
    /// decision differs from [`CoreError::RoomNotFound`] (code 4) on
    /// purpose: 4 = "unknown/temporary, may retry", 12 = "definitively
    /// over — return to the lobby, never retry".
    #[error(
        "room {0} is retired (the match ended or the room was decommissioned); do not retry — return to the lobby"
    )]
    RoomRetired(u64),

    /// A resume attempt lost the epoch guard (§7): a newer session
    /// already rebound the parked identity. Normal rejection — the client
    /// should re-authenticate; it can never double-bind a second entity
    /// for one identity through this path.
    #[error("resume rejected: a newer session already took over this parked identity")]
    ResumeStale,

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
