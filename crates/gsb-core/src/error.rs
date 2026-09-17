//! Core error types.

#[cfg(test)]
mod tests;

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

impl CoreError {
    /// The wire error class a control-plane failure is reported to the
    /// client as. The join reply in the connection actor
    /// (`conn/actor/room.rs`) is the one place a `CoreError` reaches the
    /// wire.
    ///
    /// The match is EXHAUSTIVE on purpose: adding a `CoreError` variant
    /// stops compiling here until someone chooses its wire code, so a
    /// new variant cannot silently inherit a number nobody decided on.
    /// The `_ =>` catch-all this replaces is exactly what let a variant
    /// ship unconsidered.
    ///
    /// Every number is unchanged from that catch-all's behaviour:
    /// `RoomFull` was 8, `RoomRetired` was 12, everything else was 4.
    /// `RoomOpFailed` is the honest class for that "everything else" —
    /// its documented client decision is "temporary or unknown, read the
    /// `message`, you may retry", which is what a stale resume, a
    /// tick-rate mismatch and a vanished room all are from the client's
    /// side.
    ///
    /// Never returns [`gsb_protocol::base::ErrorCode::Unspecified`];
    /// `base.proto` states that as a protocol guarantee.
    pub fn wire_code(&self) -> gsb_protocol::base::ErrorCode {
        use gsb_protocol::base::ErrorCode;
        match self {
            // A full room is a GENTLE reject with its own class: the
            // connection stays alive and the client picks another room
            // (see `conn/actor/room.rs` for why closing instead would
            // produce a reconnect storm against an already-busy server).
            Self::RoomFull(_) => ErrorCode::RoomFull,
            // "Definitively over — return to the lobby, never retry"
            // (RECONNECT §8): the decision class 4 cannot express.
            Self::RoomRetired(_) => ErrorCode::RoomRetired,
            Self::RoomNotFound(_)
            | Self::NotInRoom
            | Self::RoomExists(_)
            | Self::RoomConflict(_)
            | Self::TickRate { .. }
            | Self::KeepaliveRate { .. }
            | Self::InvalidTickRate { .. }
            | Self::RoomGone
            | Self::ResumeStale
            | Self::Protocol(_)
            | Self::Io(_) => ErrorCode::RoomOpFailed,
        }
    }
}

impl From<gsb_protocol::ProtoError> for CoreError {
    fn from(e: gsb_protocol::ProtoError) -> Self {
        Self::Protocol(e.to_string())
    }
}
