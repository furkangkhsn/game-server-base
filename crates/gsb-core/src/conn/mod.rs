//! The connection actor: one per client connection.
//!
//! Three tasks cooperate per connection (the "1 reader + 1 writer" pattern):
//!
//! ```text
//! socket read half ──▶ [reader pump] ──InMsg::Frame──▶ connection actor ◀──RegistryMsg── registry
//!                                                        │  (mailbox-driven; its
//! socket write half ◀── [writer pump] ◀──FrameBody─────┘   only await is recv)
//! room broadcast fan-out channels ─────────────────────────┘
//! ```
//!
//! The actor decodes the envelope, runs the auth/join/leave state machine
//! against the registry, and forwards game-band opcodes to the room's
//! per-connection action channel (non-blocking `try_send`: a flooding
//! client drops its own input, never stalls the actor or the room). It
//! never multiplexes: every branch is a channel receive.
//!
//! **Protocol violation budget** (the anti-amplification guardrail): the
//! `reply_err` funnel — the single exit for every protocol error this
//! actor answers — keeps a small budget in actor-local state. Each
//! violation has a *class*:
//!
//! - **hard** (weight `HARD_VIOLATION_WEIGHT`): no legitimate client
//!   path reaches it — an unknown opcode, a malformed payload, an auth
//!   state violation (out-of-order or double AUTH);
//! - **race** (weight `RACE_VIOLATION_WEIGHT`): a legitimate
//!   ~1-RTT-wide transition can produce it — the room was destroyed and
//!   an in-flight action arrives room-less, an action races a LEAVE, the
//!   leave→rejoin window strays, or the room ended the membership on its
//!   own (the input-idle ceiling's default action, B40: the client learns
//!   it from this very answer);
//! - **not a violation** (weight 0): server-side conditions (registry
//!   gone during shutdown, message-table type mismatch) — answered as
//!   before, never counted; the budget is for *client* violations.
//!
//! The first `VIOLATION_ANSWER_LIMIT` violations are answered with an
//! `ERROR` frame (diagnosis for the client developer); after that the
//! funnel goes **silent** — every further violation is counted but
//! unanswered, which bounds the amplification (a fire-and-forget ~10-byte
//! client packet can no longer buy unlimited server allocation + encode +
//! queue cost). When the weighted lifetime score reaches
//! `VIOLATION_BUDGET` the connection is **closed** (an `ERROR` code 9
//! with the reason, then the normal teardown cascade) and the close is
//! reported with the peer address (the actor carries it — see `peer`) so
//! a layer outside the server (firewall, fail2ban, future auth) can act.
//!
//! Lifetime total (not a sliding window): a legitimate connection
//! accumulates only a handful of race-class stray packets across its whole
//! life, so a budget sized well above that churn can never be exhausted by
//! honest traffic — while sustained violation traffic (the measured
//! incident: 50 rejected clients, 1550 answered errors in 8 s) exhausts it
//! within seconds. A sliding window would need per-violation timestamps
//! for no gain: the failure mode it addresses (many early violations,
//! then honesty) does not occur per-connection.
//!
//! **Pre-auth rate limits** (docs/SECURITY.md §3, the second guardrail
//! family in this file): three more mechanisms live in the same actor-local
//! state, reusing the budget machinery above instead of adding any new
//! enforcement path:
//!
//! - **AUTH attempt window** (§3.1): at most three `AUTH` attempts per ten
//!   seconds per connection; an attempt past the window's allowance is a
//!   *hard* violation and rides the budget above (four of them close).
//!   Attempts one-to-three keep the ordinary ticket-rejection path (ERROR
//!   code 10, connection alive) — a legitimate client retrying a rejected
//!   ticket is never budgeted for trying.
//! - **Heartbeat ACK throttle** (§3.2): a heartbeat ACK is answered at
//!   most once per second, in BOTH phases; surplus heartbeats are counted
//!   in a dedicated counter (one per phase — see the two fields) and NOT
//!   answered — deliberately *not* violations (a buggy-but-honest client
//!   must not burn its budget on liveness probes). The clock spans the
//!   auth boundary with a single reset at auth success, so the first
//!   heartbeat of an authenticated session is always answered.
//! - **Pre-auth frame budget** (§3.3): at most 64 inbound frames of any
//!   kind before auth success; crossing the budget closes the connection
//!   immediately (`ERROR` code 9 naming the policy). Auth success retires
//!   the counter's relevance naturally: it only gates the WaitingAuth
//!   phase.
//!
//! **Input gate** (docs/SECURITY.md, "post-auth input volume"; BACKLOG
//! E1): an opt-in per-connection token bucket over VALID game-band input,
//! set by the room joined (`RoomConfig::input_rate`, handed over with the
//! action channel on the `Seat`). Off by default. Over the rate, the
//! action is dropped in this actor before the room sees it, counted
//! (`ConnSample::input_rate_limited`) and NOT budgeted as a violation.
//! Control frames and RPC requests pass untouched; the protocol checks
//! (unknown opcode, not in a room) run before the gate and keep their
//! answers. The bucket belongs to the connection and survives room hops
//! (re-tuned, never refilled, by a join) — see the `gate` module.

mod actor;
mod close;
mod gate;
pub(crate) use gate::InputGate;
mod kind;
pub use kind::FrameKind;

pub use actor::ConnectionActor;
pub use close::ServerClose;

use std::time::Duration;

use gsb_protocol::{FrameBody, ProtoError};

use crate::id::RoomId;

/// How often an active connection flushes its wire-byte counters as a
/// sample (a connection with no inbound frames does not flush until its
/// final flush at close — it has nothing new to report in the meantime).
const METRICS_FLUSH_EVERY: Duration = Duration::from_millis(500);

/// How many violations (of any class) are *answered* with an `ERROR`
/// frame before the funnel goes silent for the rest of the connection.
/// The first few answers are the diagnosis the client developer needs;
/// every answer after that is pure amplification surface.
const VIOLATION_ANSWER_LIMIT: u32 = 3;

/// Weighted lifetime score at which the connection is closed. Sized
/// against *legitimate* churn: race-class transitions (room destroyed
/// under in-flight input, leave/rejoin windows) produce 1-3 stray packets
/// each, so even a connection that churns through several of them stays
/// far below 16 — while a sustained violator (the measured incident ran
/// ~4 stray game-band packets/s per rejected client) reaches it in a
/// handful of seconds. See the module docs for the lifetime-total
/// rationale.
const VIOLATION_BUDGET: u32 = 16;

/// Score a hard violation adds (no legitimate path reaches it, so each
/// occurrence is strong evidence of a broken or hostile client).
const HARD_VIOLATION_WEIGHT: u32 = 4;

/// Score a race-class violation adds (legitimate transitions reach it, so
/// each occurrence is weak evidence).
const RACE_VIOLATION_WEIGHT: u32 = 1;

/// The AUTH-attempt sliding window (docs/SECURITY.md §3.1): attempts
/// older than this fall out of the allowance.
///
/// Why ten seconds cannot touch honest traffic: a legitimate retry of a
/// rejected ticket is paced by fetching a FRESH ticket from the platform
/// (a round trip through the matchmaker/auth service), and a client that
/// keeps failing surfaces the error to its user instead of spinning —
/// three attempts in ten seconds is already generous headroom above any
/// real retry loop, while a scripted credential-stuffing loop hits the
/// wall on its fourth attempt.
const AUTH_WINDOW: Duration = Duration::from_secs(10);

/// AUTH attempts admitted per [`AUTH_WINDOW`] per connection (§3.1).
/// Attempts one-to-three are processed normally (ticket rejection or
/// success); attempt four-plus in-window is a HARD violation riding the
/// existing budget (weight 4 → four of them close the connection), so no
/// second enforcement mechanism exists for the flood case.
const AUTH_ATTEMPTS_PER_WINDOW: usize = 3;

/// Minimum spacing between ANSWERED heartbeat ACKs, in BOTH connection
/// phases (docs/SECURITY.md §3.2).
///
/// One answer per second is the whole guardrail: a well-behaved client
/// heartbeats at about this cadence, so its ACKs — and the RTT it reads
/// off `HeartbeatAck.tick` — are untouched, while a client sending
/// thousands per second gets exactly one answer per interval and buys
/// nothing with the rest. That is the 1:1 request/response amplification
/// this closes, and it is the same shape before and after auth: a
/// successful AUTH proves who the peer is, not that its heartbeat timer
/// is sane.
///
/// The clock is per connection and spans the auth boundary, with ONE
/// reset at auth success (see `handle_auth`) — the same phase boundary
/// that retires the §3.3 frame budget. That grants exactly one extra
/// answer over a connection's entire life (AUTH succeeds once; a second
/// one is a hard violation), so it is not a lever, and it keeps the
/// guarantee every post-auth flow relies on: the first heartbeat after a
/// successful AUTH is always answered.
const HEARTBEAT_ACK_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Pre-auth total inbound frame budget (docs/SECURITY.md §3.3): at most
/// this many frames of ANY kind before auth success; crossing it closes
/// the connection immediately (`ERROR` code 9 naming the policy).
///
/// Sized against the legitimate handshake: AUTH (plus a few ticket
/// retries), then JOIN, then heartbeats at their ~10 s cadence sums to
/// single digits even for a slow client stuck in `WaitingAuth` — while an
/// unauthenticated sender that keeps producing frames after the server's
/// answers has no honest reason to exist, and the budget stops its
/// control-frame generation from being free. Auth success retires the
/// counter's relevance (it only gates the WaitingAuth phase); post-auth
/// traffic is bounded by the action-channel and violation machinery
/// instead.
const PREAUTH_FRAME_BUDGET: u32 = 64;

/// The violation class of a protocol error, as counted by the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViolationClass {
    /// No legitimate client path reaches this error.
    Hard,
    /// A legitimate ~1-RTT transition can produce this error (room gone
    /// under in-flight input, action racing a LEAVE, leave→rejoin window).
    Race,
    /// Server-side condition, not a client violation (registry gone,
    /// message-table type mismatch): answered, never counted.
    None,
}

impl ViolationClass {
    fn weight(self) -> u32 {
        match self {
            Self::Hard => HARD_VIOLATION_WEIGHT,
            Self::Race => RACE_VIOLATION_WEIGHT,
            Self::None => 0,
        }
    }
}

/// Classify a protocol error for the violation budget. The split follows
/// the state machine, not the error text: `NotInRoom` is the *only* error
/// a legitimate client can hit in flight (its state and the server's
/// briefly disagree across join/leave/destroy transitions); every other
/// error means the client sent something no correct client sends.
fn violation_class(e: &ProtoError) -> ViolationClass {
    match e {
        ProtoError::NotInRoom => ViolationClass::Race,
        ProtoError::Other(_) => ViolationClass::None,
        // UnknownOpcode / Decode / MalformedFrame / NotAuthenticated /
        // AlreadyAuthenticated / RoomNotFound: hard.
        _ => ViolationClass::Hard,
    }
}

/// Messages addressed to the connection actor.
#[derive(Debug)]
pub enum ConnIn {
    /// A frame decoded from the network (envelope intact).
    Frame(FrameBody),
    /// The peer closed or the socket errored; the actor should clean up.
    /// A CLIENT-side end: never counted as a server close.
    Closed { reason: String },
    /// The transport refused the inbound byte stream (the reader pump's
    /// `InvalidData` exit: an oversized or undecodable frame, a WebSocket
    /// protocol violation, a corrupt TLS record). The SERVER's verdict,
    /// counted as [`ServerClose::StreamRejected`]: the actor answers it
    /// like [`Self::ServerClosed`] — an `ERROR` frame (code 9, the reason
    /// in the message) — except that the notice is best effort and never
    /// waits (the inbound stream is untrustworthy past this point, and
    /// its sender is not a peer to wait on), then cleans up.
    StreamRejected { reason: String },
    /// The server is closing this connection on its own initiative (idle
    /// timeout, write stall, connection capacity, …; `cause` says which —
    /// see [`ServerClose`]). The actor replies with an `ERROR` frame (code
    /// 9, the reason as the message) so the client can tell a server
    /// decision apart from a network failure, then cleans up.
    ServerClosed { cause: ServerClose, reason: String },
    /// The room the connection was in got destroyed.
    RoomGone(RoomId),
    /// The room ended this connection's membership on its own and the
    /// connection STAYS: the input-idle ceiling under the default
    /// `afk_action = leave_room` (BACKLOG B40, `docs/RECONNECT.md` §16).
    /// Sent by the registry after it settled its row, so the connection
    /// is authenticated and in no room from here on — as after its own
    /// `LEAVE_ROOM_REQ` — and its next join goes straight through (the
    /// implicit resume, for a parked entity). Nothing goes on the wire.
    ///
    /// Acted on only while the connection is still in `room` on the
    /// action channel that membership handed it, now closed by the room:
    /// a notice that arrives after the client left and joined again
    /// (a new membership, a new open channel) is stale and ignored.
    LeftRoom { room: RoomId },
    /// Server-wide shutdown.
    Shutdown,
    /// Server-wide shutdown, sent to a connection whose verdict the
    /// registry had decided earlier but could not queue in place (its
    /// inbox was full: the verdict waits in a spawned sender, BACKLOG
    /// F60). The stop may overtake it. Read first, it is the stop — and
    /// the verdict it carries is the one the session lost (counted once,
    /// at the connection's end, under this reason); the verdict itself,
    /// behind it or refused at the end, is not counted again. Read
    /// behind the verdict, it is never read at all: the verdict ended
    /// the session.
    ShutdownOvertaking(ServerClose),
}

impl ConnIn {
    /// The server verdict this message carries — the reason its session
    /// would book in `server_closes` — or `None` for a message that is
    /// not one (a frame, the client's end, a membership notice, the
    /// stop — also the stop that overtook a verdict: the verdict is
    /// counted by whoever reads that notice, F60). What a lost message
    /// costs (F56, F58).
    pub(crate) fn verdict(&self) -> Option<ServerClose> {
        match self {
            Self::ServerClosed { cause, .. } => Some(*cause),
            Self::RoomGone(_) => Some(ServerClose::RoomGone),
            Self::StreamRejected { .. } => Some(ServerClose::StreamRejected),
            Self::Frame(_)
            | Self::Closed { .. }
            | Self::LeftRoom { .. }
            | Self::Shutdown
            | Self::ShutdownOvertaking(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    WaitingAuth,
    Authed,
    InRoom { room: RoomId },
}
