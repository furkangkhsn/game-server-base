//! Why the SERVER ended a session: the close-reason taxonomy.
//!
//! Every path on which the server decides, on its own initiative, that a
//! session is over carries one of these reasons to the connection actor,
//! which counts it exactly once at its single exit (the final metrics
//! flush — see [`crate::metrics::ServerCloses`]). A session the CLIENT
//! ended (EOF, RST, a WebSocket close handshake, a TLS `close_notify`)
//! carries no reason and is never counted here: the counter family
//! answers "which sessions did the server shed while it kept running",
//! and a peer leaving is not shedding.
//!
//! Deliberately NOT a reason:
//!
//! - **Server shutdown** (`ConnIn::Shutdown`). It is not a verdict on
//!   the session, and it is unobservable anyway: the metrics collector
//!   exits with the ticker in the same teardown, so these closes would
//!   land (or not) depending on scheduling — and a counter whose value
//!   depends on the race it is racing is worse than none.
//! - **Ticket / protocol-version rejections.** Both keep the connection
//!   open (ERROR code 10 / 13); only their FLOOD closes, and that close
//!   is [`ServerClose::ViolationBudget`].
//! - **The input-idle ceiling** (`max_idle_input_secs`) under its
//!   default `afk_action = leave_room`. It hands the ENTITY to the
//!   disconnect policy and ends the membership (`ConnIn::LeftRoom`); the
//!   transport session is not ended by it. Under
//!   the opt-in `afk_action = disconnect` it IS a verdict — the room asks
//!   the registry to close the session — and is counted as
//!   [`ServerClose::IdleInput`].
//! - **A game's kick** (`TickCtx::kick`, BACKLOG E8) is a verdict too —
//!   the game ended the membership and the room asked the registry to
//!   close the session — and is counted as [`ServerClose::Kicked`]. The
//!   game decided WHY; the counter only says that a game did.

/// One reason the server ended a session. The order of [`Self::ALL`] is
/// the order every export uses (the loadgen wire codec, the log line,
/// the Prometheus family), so a new reason is appended, never inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerClose {
    /// No inbound traffic for the idle window: the stream reader pump's
    /// deadline, or the rUDP demux's idle sweep (the same guardrail on a
    /// transport with no per-connection socket).
    IdleTimeout,
    /// The writer pump's progress clock: nothing written to the socket
    /// for the write-stall window while there was something to write.
    WriteStall,
    /// The rUDP reliable control band is dead: no ACK progress for the
    /// liveness bound, or the retransmit backlog cap crossed.
    RelDead,
    /// The connection actor's weighted protocol-violation budget was
    /// exhausted.
    ViolationBudget,
    /// The pre-auth total frame budget (SECURITY §3.3) was crossed.
    PreauthBudget,
    /// The transport refused the inbound byte stream: a frame over
    /// `max_frame_bytes`, an undecodable frame body, a WebSocket
    /// protocol violation, a corrupt TLS record — the reader pump's
    /// `InvalidData` exit. Answered with a best-effort `ERROR` code 9
    /// like the other verdicts (the WebSocket door's own close frame
    /// takes its place there: no data frame may follow a close frame).
    StreamRejected,
    /// Refused at birth: the server is at `max_connections`.
    ConnCap,
    /// Refused at birth: the server is at `max_unauth_conns` (§4).
    UnauthCap,
    /// Evicted by a newer session of the same identity in the same room
    /// ("latest wins").
    Superseded,
    /// The room the session was in was destroyed or died under it.
    RoomGone,
    /// The outbound channel was found CLOSED (the writer pump exited)
    /// with no verdict on record. Mostly the tail of a peer that vanished
    /// between the writer's failed write and the reader's report; a
    /// write stall or a peer EOF that is already waiting in the mailbox
    /// is attributed to ITS reason instead (see the actor's
    /// `adopt_pending_close`).
    OutboundDead,
    /// The room's input-idle ceiling (`max_idle_input_secs`) with the
    /// opt-in `afk_action = disconnect`: the member stopped PLAYING (its
    /// transport was alive), the room ended its membership and asked the
    /// registry to close the connection (BACKLOG E6,
    /// `docs/RECONNECT.md` §16). Unlike [`Self::IdleTimeout`] — no
    /// inbound bytes at all — a heartbeating client reaches this one.
    /// Announced with a best-effort, never-waiting `ERROR` code 9.
    IdleInput,
    /// The GAME kicked the member (`TickCtx::kick`, BACKLOG E8,
    /// `docs/RECONNECT.md` §16.3): the room ended its membership through
    /// the ordinary disconnect policy and asked the registry to close the
    /// connection, with the game's reason. Announced like
    /// [`Self::IdleInput`] — a best-effort, never-waiting `ERROR` code 9
    /// whose `message` is `kicked: <the game's reason>`.
    Kicked,
}

impl ServerClose {
    /// Number of reasons.
    pub const COUNT: usize = 13;

    /// Every reason, in export order.
    pub const ALL: [ServerClose; Self::COUNT] = [
        Self::IdleTimeout,
        Self::WriteStall,
        Self::RelDead,
        Self::ViolationBudget,
        Self::PreauthBudget,
        Self::StreamRejected,
        Self::ConnCap,
        Self::UnauthCap,
        Self::Superseded,
        Self::RoomGone,
        Self::OutboundDead,
        Self::IdleInput,
        Self::Kicked,
    ];

    /// Position in [`Self::ALL`] (the counter array index). An exhaustive
    /// match, so a new variant does not compile until it has a slot.
    pub const fn index(self) -> usize {
        match self {
            Self::IdleTimeout => 0,
            Self::WriteStall => 1,
            Self::RelDead => 2,
            Self::ViolationBudget => 3,
            Self::PreauthBudget => 4,
            Self::StreamRejected => 5,
            Self::ConnCap => 6,
            Self::UnauthCap => 7,
            Self::Superseded => 8,
            Self::RoomGone => 9,
            Self::OutboundDead => 10,
            Self::IdleInput => 11,
            Self::Kicked => 12,
        }
    }

    /// The stable export label (`reason="…"` on Prometheus, the key
    /// suffix on the log line and the loadgen `RESULT` line).
    pub const fn label(self) -> &'static str {
        match self {
            Self::IdleTimeout => "idle_timeout",
            Self::WriteStall => "write_stall",
            Self::RelDead => "rel_dead",
            Self::ViolationBudget => "violation_budget",
            Self::PreauthBudget => "preauth_budget",
            Self::StreamRejected => "stream_rejected",
            Self::ConnCap => "conn_cap",
            Self::UnauthCap => "unauth_cap",
            Self::Superseded => "superseded",
            Self::RoomGone => "room_gone",
            Self::OutboundDead => "outbound_dead",
            Self::IdleInput => "idle_input",
            Self::Kicked => "kicked",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL`, `index` and `label` must agree: every reason at its own
    /// slot, every label distinct (a duplicated label would merge two
    /// Prometheus series into one).
    #[test]
    fn all_index_and_label_agree() {
        for (i, r) in ServerClose::ALL.iter().enumerate() {
            assert_eq!(r.index(), i, "{r:?} is not at its own slot");
        }
        let mut labels: Vec<&str> = ServerClose::ALL.iter().map(|r| r.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), ServerClose::COUNT, "labels must be distinct");
    }
}
