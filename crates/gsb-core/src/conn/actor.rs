//! The connection actor's state. Its behaviour is split across child
//! modules by concern — they are children, not siblings, so the
//! actor's fields stay private to this module tree.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use tokio::sync::mpsc;

use gsb_protocol::MessageTable;

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::conn::*;
use crate::id::ConnectionId;
use crate::metrics::MetricsEvent;
use crate::registry::RegistryMsg;
use crate::room::Action;

mod auth;
mod close;
mod frame;
mod lifecycle;
mod room;
mod violation;

/// The connection actor.
pub struct ConnectionActor {
    conn: ConnectionId,
    /// The peer's socket address. The transport knows it at accept time
    /// and passes it in here — the only way the *close signal* can carry
    /// an address without a shared conn→addr table (the mapping lives in
    /// the accept loop's head today and is never exported; a lookup API
    /// would need either shared state or an awaited round trip, both
    /// banned by the architecture). Carrying it is 16-24 bytes of
    /// actor-local state and keeps the signal self-contained for an
    /// external layer (firewall/fail2ban/future auth).
    peer: SocketAddr,
    state: ConnState,
    table: std::sync::Arc<MessageTable>,
    registry: Mailbox<RegistryMsg>,
    inbox: Inbox<ConnIn>,
    /// To the writer pump; also cloned to the room for fan-out.
    out: Mailbox<FrameBatch>,
    /// The room's per-connection action channel (set on join, cleared on
    /// leave / room-gone). Game-band opcodes are `try_send`-ed here.
    actions: Option<Mailbox<Action>>,
    /// Local wire-byte counters (see [`crate::metrics`]); flushed as
    /// deltas — on inbound frames at most once per `METRICS_FLUSH_EVERY`
    /// and a final time at close. The dominant outbound traffic (room
    /// fan-out) is counted by the room, not here.
    m_in_bytes: u64,
    m_in_frames: u64,
    m_out_bytes: u64,
    m_out_frames: u64,
    /// Input actions this actor dropped on a full (bounded) per-connection
    /// action channel, delta since the last flush. This is the *only*
    /// input-loss point in the architecture (the room's READ phase is a
    /// bounded pull and never drops — see `room::RoomCounters`), and the
    /// drop is attributed to the sender: a flooding connection drops its
    /// own input, never another connection's.
    m_actions_dropped: u64,
    /// Warned once about action drops (the drop *count* is in the metrics
    /// samples; per-drop warnings would flood the log exactly when a
    /// flooder is doing what it does).
    m_actions_dropped_warned: bool,
    /// Valid game-band input refused over the room's input rate limit,
    /// delta since the last flush (docs/SECURITY.md, "post-auth input
    /// volume"). Its own counter, NOT a violation (see
    /// `ConnSample::input_rate_limited`).
    m_input_limited: u64,
    /// Violation budget state (all actor-local — the counter *is* the
    /// budget; see the module docs): weighted lifetime score, raw event
    /// count (for the close signal and metrics), how many violations have
    /// been answered so far (capped at [`VIOLATION_ANSWER_LIMIT`]), and
    /// the closing flag the run loop checks after each frame.
    v_score: u32,
    v_events: u32,
    v_answered: u32,
    v_closing: bool,
    /// Violation events since the last metrics flush (delta, like the
    /// other conn counters) → `NetReport.violations`.
    m_violations: u64,
    /// AUTH attempt timestamps inside the current [`AUTH_WINDOW`] (§3.1).
    /// Actor-local, bounded by construction: only ADMITTED attempts are
    /// recorded (at most [`AUTH_ATTEMPTS_PER_WINDOW`] entries, ever), so
    /// the flood itself cannot grow this — a violator's extra attempts are
    /// answered through the budget above and never touch the window.
    /// Admitted-only recording also means an honest client's ten quiet
    /// seconds always fully restore its allowance.
    auth_attempts: VecDeque<Instant>,
    /// When the last heartbeat ACK went out (§3.2). ONE clock for the
    /// whole connection — the throttle is one rate limiter, not two —
    /// reset once at auth success so the first heartbeat of an
    /// authenticated session is always answered.
    last_hb_ack: Option<Instant>,
    /// Surplus PRE-AUTH heartbeats counted silently (§3.2): over-rate
    /// liveness probes that got no ACK. A dedicated counter, deliberately
    /// NOT a violation despite the contract's "counted" wording: the
    /// throttle itself already caps the amplification (one small ACK per
    /// second per connection), so budgeting adds nothing on the hostile
    /// side — it would only convert a buggy-but-honest client (a
    /// misconfigured heartbeat timer in a lobby screen) into a forced
    /// disconnect. Liveness probing is not evidence of hostility; a real
    /// frame flood pre-auth is closed by the §3.3 budget anyway.
    m_preauth_hb_extra: u64,
    /// The same, POST-auth. Kept separate from the field above rather
    /// than merged, because the two answer different questions with
    /// different remedies: a surplus from an UNAUTHENTICATED peer is the
    /// §3.2 security signal (that peer is also under the §3.3 frame
    /// budget and the §4 unauth cap, and the operator's move is to
    /// tighten a pre-auth guardrail), while a surplus from a peer that
    /// has proven who it is says a known client's heartbeat timer is
    /// misconfigured — a bug report, not an attack. Merging them would
    /// cost §3.2's counter the ability to answer its own question. The
    /// mechanism above them is shared; only the attribution is not.
    m_hb_extra: u64,
    /// Inbound frames since the connection opened, while still
    /// `WaitingAuth` (§3.3). Auth success retires its relevance: every
    /// check is gated on the WaitingAuth state, so nothing to reset.
    preauth_frames: u32,
    /// Set when the §3.3 pre-auth frame budget closes the connection (the
    /// close notice was sent); checked by the run loop next to
    /// `v_closing`. Separate from the violation flag because this close is
    /// a capacity policy, not a scored violation — but the teardown is the
    /// ordinary cascade either way.
    p_closing: bool,
    /// Set when a write to the outbound channel found it CLOSED — the
    /// writer pump exited (a socket write failed, or the write-stall
    /// window ran out), so this connection can never receive another
    /// byte. Which of the two it was is read off the mailbox at exit
    /// (`adopt_pending_close`). Checked by the run loop
    /// next to `v_closing`. Unlike those two this is not a policy: it is
    /// the discovery that the session is already half-dead, and the only
    /// honest response is the ordinary teardown (no close notice is sent —
    /// there is nothing left to send it through).
    w_closing: bool,
    /// Why the SERVER ended this session, when it did (see
    /// [`ServerClose`]); `None` for a client-side end. The first verdict
    /// wins — a close notice that then fails to send (`w_closing`) does
    /// not overwrite the verdict that sent it. Reported once, on the
    /// final metrics flush.
    server_close: Option<ServerClose>,
    m_flushed_in_bytes: u64,
    m_flushed_in_frames: u64,
    m_flushed_out_bytes: u64,
    m_flushed_out_frames: u64,
    /// Metric samples dropped on a full (bounded) metrics channel since the
    /// last flush (delta, like the other conn counters).
    m_metrics_dropped: u64,
    m_last_flush: Instant,
    /// Outbound metrics path (bounded channel; the actor sends with the
    /// synchronous `try_send` — the actor's only await stays the inbox
    /// `recv`).
    metrics: mpsc::Sender<MetricsEvent>,
    /// The server's ticket-validation hook (see `crate::auth`); `None` =
    /// the legacy local-auth path (the `Auth.name` is accepted as-is and
    /// every pre-hook flow is byte-for-byte unchanged).
    auth: Option<crate::auth::TicketAuth>,
    /// The identity the last successful ticket validation resolved to
    /// (set on ticket-auth success; `None` on the local-auth path). Pins
    /// the room for the next `JOIN_ROOM_REQ` (a different room is a
    /// normal rejection — ERROR code 11).
    ticket: Option<crate::auth::ValidatedTicket>,
    /// This connection's resume key: `ValidatedTicket.player` on the
    /// ticket path, `Auth.name` on the local-auth path (which makes
    /// resume demo/testing-only there — no authority behind the name;
    /// noted at the ledger site too). Empty until a successful AUTH.
    /// Rides every `SpawnPlayer` as the implicit-resume key of §14.3.
    identity: String,
}
