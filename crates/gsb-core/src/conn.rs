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
//! - **hard** (weight [`HARD_VIOLATION_WEIGHT`]): no legitimate client
//!   path reaches it — an unknown opcode, a malformed payload, an auth
//!   state violation (out-of-order or double AUTH);
//! - **race** (weight [`RACE_VIOLATION_WEIGHT`]): a legitimate
//!   ~1-RTT-wide transition can produce it — the room was destroyed and
//!   an in-flight action arrives room-less, an action races a LEAVE, or
//!   the leave→rejoin window strays;
//! - **not a violation** (weight 0): server-side conditions (registry
//!   gone during shutdown, message-table type mismatch) — answered as
//!   before, never counted; the budget is for *client* violations.
//!
//! The first [`VIOLATION_ANSWER_LIMIT`] violations are answered with an
//! `ERROR` frame (diagnosis for the client developer); after that the
//! funnel goes **silent** — every further violation is counted but
//! unanswered, which bounds the amplification (a fire-and-forget ~10-byte
//! client packet can no longer buy unlimited server allocation + encode +
//! queue cost). When the weighted lifetime score reaches
//! [`VIOLATION_BUDGET`] the connection is **closed** (an `ERROR` code 9
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
//! - **Pre-auth heartbeat throttle** (§3.2): before auth success a
//!   heartbeat ACK is answered at most once per second; surplus heartbeats
//!   are counted in a dedicated counter and NOT answered — deliberately
//!   *not* violations (see the field's doc: a buggy-but-honest client must
//!   not burn its budget on liveness probes). After auth success
//!   heartbeats keep their exact pre-existing behavior.
//! - **Pre-auth frame budget** (§3.3): at most 64 inbound frames of any
//!   kind before auth success; crossing the budget closes the connection
//!   immediately (`ERROR` code 9 naming the policy). Auth success retires
//!   the counter's relevance naturally: it only gates the WaitingAuth
//!   phase.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::{debug, warn};

use gsb_protocol::op;
use gsb_protocol::{FrameBody, MessageTable, ProtoError, base};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::metrics::{ConnSample, MetricsEvent};
use crate::registry::RegistryMsg;
use crate::room::Action;

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

/// Minimum spacing between ANSWERED pre-auth heartbeat ACKs
/// (docs/SECURITY.md §3.2). One answer per second still proves liveness
/// to an honest waiting-in-lobby client; anything faster pre-auth is the
/// 1:1 amplification shape the cap exists to close. Post-auth heartbeats
/// are throttled by nothing (the liveness signal must stay intact).
const PREAUTH_HEARTBEAT_MIN_INTERVAL: Duration = Duration::from_secs(1);

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
    Closed { reason: String },
    /// The server is closing this connection on its own initiative (idle
    /// timeout, connection capacity). The actor replies with an `ERROR`
    /// frame (code 9, the reason as the message) so the client can tell a
    /// server decision apart from a network failure, then cleans up.
    ServerClosed { reason: String },
    /// The room the connection was in got destroyed.
    RoomGone(RoomId),
    /// Server-wide shutdown.
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    WaitingAuth,
    Authed,
    InRoom { room: RoomId },
}

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
    /// When the last pre-auth heartbeat ACK went out (§3.2). Consulted
    /// only while `WaitingAuth`; post-auth heartbeats answer unconditionally.
    last_preauth_hb_ack: Option<Instant>,
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

impl ConnectionActor {
    // The actor's wiring (the base's actor constructors take every
    // mailbox the actor owns — the `new` is the composition point).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conn: ConnectionId,
        // The peer's address (for the violation-close signal; see the
        // `peer` field). The accept loop learns it from the transport.
        peer: SocketAddr,
        table: std::sync::Arc<MessageTable>,
        registry: Mailbox<RegistryMsg>,
        inbox: Inbox<ConnIn>,
        out: Mailbox<FrameBatch>,
        // Outbound metrics path (see `crate::metrics`): a bounded channel;
        // the actor sends with the synchronous `try_send` (no await).
        metrics: mpsc::Sender<MetricsEvent>,
        // The server's ticket hook (see the field): `None` = local auth.
        auth: Option<crate::auth::TicketAuth>,
    ) -> Self {
        Self {
            conn,
            peer,
            state: ConnState::WaitingAuth,
            table,
            registry,
            inbox,
            out,
            actions: None,
            auth,
            ticket: None,
            identity: String::new(),
            m_in_bytes: 0,
            m_in_frames: 0,
            m_out_bytes: 0,
            m_out_frames: 0,
            m_actions_dropped: 0,
            m_actions_dropped_warned: false,
            v_score: 0,
            v_events: 0,
            v_answered: 0,
            v_closing: false,
            m_violations: 0,
            auth_attempts: VecDeque::new(),
            last_preauth_hb_ack: None,
            m_preauth_hb_extra: 0,
            preauth_frames: 0,
            p_closing: false,
            m_flushed_in_bytes: 0,
            m_flushed_in_frames: 0,
            m_flushed_out_bytes: 0,
            m_flushed_out_frames: 0,
            m_metrics_dropped: 0,
            m_last_flush: Instant::now(),
            metrics,
        }
    }

    /// Run until the peer is gone or the server shuts down.
    ///
    /// Note: the accept loop sends [`RegistryMsg::ConnOpened`] (it owns the
    /// sender half of this actor's inbox) *before* spawning the actor, so
    /// ordering with the first client frame is guaranteed.
    pub async fn run(mut self) {
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                ConnIn::Frame(frame) => {
                    // Metrics: count this frame's wire bytes (frame body:
                    // 2-byte op + payload) before handling it.
                    self.m_in_bytes =
                        self.m_in_bytes.saturating_add(2 + frame.payload.len() as u64);
                    self.m_in_frames += 1;
                    self.maybe_flush_metrics(false);
                    // Pre-auth total frame budget (§3.3): counted BEFORE
                    // dispatch, so the crossing frame is not processed at
                    // all — an unauthenticated peer that keeps producing
                    // frames past the budget gets the close notice instead
                    // of one more round of server work. Every check is
                    // gated on WaitingAuth, so auth success naturally ends
                    // the counting (nothing to reset).
                    if self.state == ConnState::WaitingAuth {
                        self.preauth_frames += 1;
                        if self.preauth_frames > PREAUTH_FRAME_BUDGET {
                            self.close_preauth_budget().await;
                            break;
                        }
                    }
                    self.handle_frame(frame).await;
                    // The violation budget may have been exhausted while
                    // handling the frame (the `ERROR` code 9 close notice
                    // was already sent by `reply_err`); tear down now,
                    // exactly like a `ServerClosed`. Same teardown for the
                    // §3.3 pre-auth budget (its own code-9 notice was sent
                    // by `close_preauth_budget`).
                    if self.v_closing || self.p_closing {
                        break;
                    }
                }
                ConnIn::Closed { reason } => {
                    debug!(%self.conn, %reason, "connection closed by peer/io");
                    break;
                }
                ConnIn::ServerClosed { reason } => {
                    // The server made this decision (idle timeout, server at
                    // connection capacity). Unlike a peer EOF the client may
                    // still be listening: tell it why, then clean up.
                    warn!(%self.conn, %reason, "server closing connection");
                    let _ = self
                        .send_frame(
                            op::base::ERROR,
                            &base::Error {
                                code: 9,
                                message: reason,
                            },
                        )
                        .await;
                    break;
                }
                ConnIn::RoomGone(room) => {
                    warn!(%self.conn, room = %room, "room destroyed; detaching");
                    self.detach();
                    let _ = self
                        .send_frame(
                            op::base::ERROR,
                            &base::Error {
                                code: 5,
                                message: "room destroyed".into(),
                            },
                        )
                        .await;
                    break;
                }
                ConnIn::Shutdown => {
                    debug!(%self.conn, "connection shutdown (server)");
                    break;
                }
            }
        }

        // Metrics: final flush of whatever is unflushed (marks the
        // connection's end).
        self.maybe_flush_metrics(true);

        // Cleanup: tell the registry so the player entity is despawned.
        let _ = self
            .registry
            .send(RegistryMsg::ConnClosed { conn: self.conn })
            .await;
    }

    /// Flush this connection's wire-byte counters as a delta sample when
    /// there is new data and the flush interval has passed (or unconditionally for the
    /// final flush). Synchronous: the only check is an `Instant`
    /// comparison on each inbound frame, so the actor's only await stays
    /// the inbox `recv`.
    fn maybe_flush_metrics(&mut self, last: bool) {
        let in_b = self.m_in_bytes - self.m_flushed_in_bytes;
        let in_f = self.m_in_frames - self.m_flushed_in_frames;
        let out_b = self.m_out_bytes - self.m_flushed_out_bytes;
        let out_f = self.m_out_frames - self.m_flushed_out_frames;
        let adrops = self.m_actions_dropped;
        let drops = self.m_metrics_dropped;
        let viols = self.m_violations;
        if in_b == 0
            && in_f == 0
            && out_b == 0
            && out_f == 0
            && adrops == 0
            && drops == 0
            && viols == 0
        {
            return;
        }
        if !last && Instant::now().duration_since(self.m_last_flush) < METRICS_FLUSH_EVERY {
            return;
        }
        self.m_flushed_in_bytes = self.m_in_bytes;
        self.m_flushed_in_frames = self.m_in_frames;
        self.m_flushed_out_bytes = self.m_out_bytes;
        self.m_flushed_out_frames = self.m_out_frames;
        self.m_actions_dropped = 0;
        self.m_metrics_dropped = 0;
        self.m_violations = 0;
        self.m_last_flush = Instant::now();
        // A3: bounded channel + synchronous `try_send`. On a full channel the
        // sample is dropped (harmless — the counters are cumulative deltas and
        // the next flush or the final flush carries the rest) and counted in
        // the *next* sample.
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.metrics.try_send(MetricsEvent::Conn(ConnSample {
                conn: self.conn,
                bytes_in: in_b,
                bytes_out: out_b,
                frames_in: in_f,
                frames_out: out_f,
                actions_dropped: adrops,
                metrics_dropped: drops,
                violations: viols,
                last,
            }))
        {
            self.m_metrics_dropped += 1;
        }
    }

    fn detach(&mut self) {
        self.state = ConnState::Authed;
        self.actions = None;
    }

    async fn handle_frame(&mut self, frame: FrameBody) {
        match frame.op {
            op::base::AUTH_REQ => {
                if self.state != ConnState::WaitingAuth {
                    self.reply_err(ProtoError::AlreadyAuthenticated).await;
                    return;
                }
                // AUTH attempt window (§3.1): admitted before ANY
                // processing. Attempts one-to-three keep the ordinary path
                // (ticket rejection = ERROR code 10, connection alive — a
                // legitimate retry is never budgeted for trying); attempt
                // four-plus in-window is a HARD violation through the same
                // funnel as every other hard error (weight 4 → four of
                // them exhaust the budget and close), so the flood case
                // adds no new enforcement machinery.
                //
                // Only ADMITTED attempts enter the window (bounded at
                // [`AUTH_ATTEMPTS_PER_WINDOW`] entries forever): a
                // violator cannot extend its own occupancy, and the budget
                // — not the window — decides when flooding stops mattering
                // (four violations close). Ten quiet seconds always fully
                // restore an honest client's allowance.
                let now = Instant::now();
                self.auth_attempts
                    .retain(|t| now.duration_since(*t) < AUTH_WINDOW);
                if self.auth_attempts.len() >= AUTH_ATTEMPTS_PER_WINDOW {
                    // Auth-family ERROR code 3 (the documented "auth"
                    // class: unauthenticated / re-auth) — no new wire
                    // vocabulary; the message carries the specificity.
                    self.count_violation(
                        ViolationClass::Hard,
                        3,
                        format!(
                            "auth attempt rate limit exceeded: max \
                             {AUTH_ATTEMPTS_PER_WINDOW} attempts per \
                             {AUTH_WINDOW:?}; wait for the window to pass"
                        ),
                    )
                    .await;
                    return;
                }
                self.auth_attempts.push_back(now);
                let auth: base::Auth = match self.decode::<base::Auth>(frame.op, frame) {
                    Ok(m) => m,
                    Err(e) => {
                        self.reply_err(e).await;
                        return;
                    }
                };
                // Ticket-auth (the control-plane hook, see `crate::auth`):
                // the hook is the identity authority. Two shapes, decided
                // by the server's configuration (not per frame):
                //
                // - NO hook (or an empty ticket on a local-auth server):
                //   the legacy path — `Auth.name` is accepted as-is.
                // - HOOK configured: the ticket must be non-empty and is
                //   validated ASYNCHRONOUSLY through the common deferred-
                //   completion mechanism: a spawned worker runs the
                //   validator (bounded by the hook's timeout) and reports
                //   to this handler over a single oneshot — the same
                //   round-trip idiom as the join above. The handler's
                //   park is bounded by the timeout, so a hung validator
                //   cannot park the actor forever (it resolves to a
                //   timeout rejection). While parked, at most ONE
                //   validation is in flight (structural: there is no
                //   concurrent frame handler) — the per-connection
                //   amplification bound (see `crate::auth`).
                if let Some(hook) = self.auth.clone() {
                    if auth.ticket.is_empty() {
                        // A ticket-auth server with no ticket: a normal
                        // rejection (the client presents one — it does not
                        // hold a valid ticket, and the connection stays
                        // alive to retry).
                        self.reply_ticket_error(
                            crate::auth::TicketError::Rejected("no ticket presented".into()),
                        )
                        .await;
                        return;
                    }
                    let (reply_tx, reply_rx) =
                        oneshot::channel::<
                            Result<crate::auth::ValidatedTicket, crate::auth::TicketError>,
                        >();
                    let ticket = auth.ticket.clone();
                    tokio::spawn(async move {
                        // The validator's own future (the platform's
                        // adapter — a signature-service call, a cache, …)
                        // wrapped in the hook's timeout: the worker cannot
                        // outlive `timeout` (a resource guard), and the
                        // actor's park above cannot outlive it either.
                        let outcome =
                            tokio::time::timeout(hook.timeout, (hook.validator)(ticket.into()))
                                .await;
                        let result = match outcome {
                            Ok(r) => r,
                            Err(_elapsed) => Err(crate::auth::TicketError::TimedOut),
                        };
                        // The send fails (the receiver is dropped) when the
                        // connection went away while validating: the actor
                        // processed its `Closed` on its next message after
                        // the park, and this worker simply exits.
                        let _ = reply_tx.send(result);
                    });
                    match reply_rx.await {
                        Ok(Ok(v)) => {
                            // Success: the hook's identity is installed (it
                            // supersedes `Auth.name`) and the ticket pins
                            // the room for the next join.
                            self.ticket = Some(v.clone());
                            self.identity = v.player.clone();
                            self.state = ConnState::Authed;
                            // §4: the connection leaves the registry's
                            // unauthenticated pool (the cap lives where the
                            // connection table lives). A failed send means
                            // the registry is gone (shutdown) — ignored,
                            // like every other fire-and-forget notice.
                            let _ = self
                                .registry
                                .send(RegistryMsg::Authed { conn: self.conn })
                                .await;
                            debug!(%self.conn, player = %v.player, room = %v.room, "ticket authenticated");
                            let _ = self
                                .send_frame(
                                    op::base::AUTH_RESULT,
                                    &base::AuthResult {
                                        ok: true,
                                        reason: String::new(),
                                        player: v.player,
                                        room: v.room.0,
                                    },
                                )
                                .await;
                        }
                        Ok(Err(e)) => {
                            // Rejected or timed out: a NORMAL rejection —
                            // the connection stays alive (ERROR code 10)
                            // and the state stays WaitingAuth (a fresh
                            // ticket may be re-presented). Never counted
                            // against the violation budget (see
                            // `crate::auth` and the ERROR code docs).
                            self.reply_ticket_error(e).await;
                        }
                        Err(_dropped) => {
                            // The worker died without reporting (a panic
                            // inside the platform's validator): a
                            // server-side condition (weight 0, like the
                            // "registry gone" arm below) — answered as a
                            // generic failure, not a violation.
                            let _ = self
                                .send_frame(
                                    op::base::ERROR,
                                    &base::Error {
                                        code: 7,
                                        message: "ticket validator unavailable".into(),
                                    },
                                )
                                .await;
                        }
                    }
                    return;
                }
                self.state = ConnState::Authed;
                // §4: same unauthenticated-pool notice as the ticket path.
                let _ = self
                    .registry
                    .send(RegistryMsg::Authed { conn: self.conn })
                    .await;
                // Local-auth path: the name IS the resume key (demo and
                // testing only — RECONNECT §4: on this path nothing
                // authoritative stands behind the name).
                self.identity = auth.name.clone();
                debug!(%self.conn, name = %auth.name, "authenticated");
                let _ = self
                    .send_frame(
                        op::base::AUTH_RESULT,
                        &base::AuthResult {
                            ok: true,
                            reason: String::new(),
                            player: String::new(),
                            room: 0,
                        },
                    )
                    .await;
            }
            op::base::JOIN_ROOM_REQ => {
                if self.state != ConnState::Authed {
                    self.reply_err(ProtoError::NotAuthenticated).await;
                    return;
                }
                let join: base::JoinRoom = match self.decode::<base::JoinRoom>(frame.op, frame) {
                    Ok(m) => m,
                    Err(e) => {
                        self.reply_err(e).await;
                        return;
                    }
                };
                let room = RoomId(join.room_id);
                // Ticket pin: a ticket-auth connection may only join the
                // room its ticket names (the platform set up THE match,
                // not "any room on this server"). A mismatch is a NORMAL
                // rejection (ERROR code 11 — the connection stays alive
                // and may join the pinned room); it is not a violation
                // (the frame is well-formed; the client simply aimed at
                // the wrong room).
                if let Some(v) = &self.ticket
                    && room != v.room
                {
                    warn!(%self.conn, room = %room, pinned = %v.room, "join rejected: ticket pins a different room");
                    let _ = self
                        .send_frame(
                            op::base::ERROR,
                            &base::Error {
                                code: 11,
                                message: format!(
                                    "ticket pins room {} (the platform-set room); \
                                     joining room {} is rejected",
                                    v.room, room
                                ),
                            },
                        )
                        .await;
                    return;
                }
                let (reply_tx, reply_rx) =
                    oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
                if self
                    .registry
                    .send(RegistryMsg::SpawnPlayer {
                        conn: self.conn,
                        room,
                        out: self.out.clone(),
                        identity: self.identity.clone(),
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    self.reply_err(ProtoError::Other("registry gone".into()))
                        .await;
                    return;
                }
                match reply_rx.await {
                    Ok(Ok((entity, actions))) => {
                        self.state = ConnState::InRoom { room };
                        self.actions = Some(actions);
                        let _ = self
                            .send_frame(
                                op::base::JOIN_ROOM_RESULT,
                                &base::JoinRoomResult { entity },
                            )
                            .await;
                        debug!(%self.conn, room = %room, entity, "joined room");
                    }
                    Ok(Err(e)) => {
                        // `RoomFull` gets its own code (8) so the client can
                        // tell "this room is full — pick another one or
                        // retry later" (8) from a permanent failure like a
                        // missing room (4). Either way the connection stays
                        // alive: a gentle reject costs one small frame and
                        // keeps the session reusable, while a silent close
                        // looks like a network failure and sends the
                        // client into a reconnect/backoff loop against a
                        // server that is (by definition) already busy.
                        // `RoomRetired` gets code 12 (§8): "definitively
                        // over — return to the lobby, never retry", the
                        // client decision ERROR 4 cannot express. A stale
                        // resume (`ResumeStale`) is an ordinary rejection:
                        // code 4 with its reason; the client re-auths and
                        // its next join falls through to a fresh join.
                        let code = match &e {
                            CoreError::RoomFull(_) => 8,
                            CoreError::RoomRetired(_) => 12,
                            _ => 4,
                        };
                        let _ = self
                            .send_frame(
                                op::base::ERROR,
                                &base::Error {
                                    code,
                                    message: e.to_string(),
                                },
                            )
                            .await;
                    }
                    Err(_) => {
                        let _ = self
                            .send_frame(
                                op::base::ERROR,
                                &base::Error {
                                    code: 4,
                                    message: "registry unavailable".into(),
                                },
                            )
                            .await;
                    }
                }
            }
            op::base::LEAVE_ROOM_REQ => {
                let ConnState::InRoom { room } = self.state else {
                    self.reply_err(ProtoError::NotInRoom).await;
                    return;
                };
                self.detach();
                let _ = self
                    .registry
                    .send(RegistryMsg::DespawnPlayer { conn: self.conn })
                    .await;
                let _ = self
                    .send_frame(op::base::LEAVE_ROOM_RESULT, &base::LeaveRoomResult {})
                    .await;
                debug!(%self.conn, room = %room, "left room");
            }
            op::base::HEARTBEAT => {
                let hb: base::Heartbeat = match self.decode::<base::Heartbeat>(frame.op, frame) {
                    Ok(m) => m,
                    Err(e) => {
                        self.reply_err(e).await;
                        return;
                    }
                };
                // Pre-auth heartbeat throttle (§3.2): at most one ACK per
                // second before auth success — the 1:1 request/response
                // amplification of unauthenticated liveness probing ends
                // here. Surplus heartbeats are counted in a dedicated
                // counter and get NO answer, deliberately NOT budgeted
                // (see the field doc: the throttle already caps the cost,
                // so scoring them would only punish honest-buggy clients).
                // Post-auth the branch below is byte-identical to the
                // pre-Tur-B behavior: every heartbeat answered (the
                // session's liveness signal).
                if self.state == ConnState::WaitingAuth {
                    let now = Instant::now();
                    let due = self.last_preauth_hb_ack.is_none_or(|t| {
                        now.duration_since(t) >= PREAUTH_HEARTBEAT_MIN_INTERVAL
                    });
                    if !due {
                        self.m_preauth_hb_extra += 1;
                        debug!(
                            %self.conn,
                            extra = self.m_preauth_hb_extra,
                            "pre-auth heartbeat over the 1/s answer rate; counted, not answered"
                        );
                        return;
                    }
                    self.last_preauth_hb_ack = Some(now);
                }
                let _ = self
                    .send_frame(
                        op::base::HEARTBEAT_ACK,
                        &base::HeartbeatAck { tick: hb.tick },
                    )
                    .await;
            }
            // Correlated request (the RPC pattern, see `crate::rpc`): a
            // room-scoped operation, so it is forwarded to the room like
            // a game-band op (the room's core decodes the base envelope
            // and owns the correlation: pending caps, timeouts, the
            // exactly-one-answer reconciliation). The actor stays thin —
            // it does not decode the envelope, so a malformed envelope is
            // a normal rejection answered by the room (the same class as
            // an undecodable game payload), never a base-band violation
            // here. Not in a room → the same `NotInRoom` race-class
            // answer as any other game op (a request racing a leave or a
            // destroyed room is a legitimate ~1-RTT stray).
            op::base::RPC_REQ => {
                self.forward_to_room(frame).await;
            }
            // Unknown *base-band* opcode: no legitimate client sends one,
            // so it is a hard protocol violation (answered + budgeted).
            // Note this is what *makes* `UnknownOpcode` reachable in the
            // funnel: the actor only decodes its control ops, so before
            // this check an unknown base-band opcode would have been
            // forwarded as an (ignored) room action.
            unknown if unknown < gsb_protocol::op::GAME_BAND_START => {
                self.reply_err(ProtoError::UnknownOpcode(unknown)).await;
            }
            // Game band: the game crate owns these opcodes (the message
            // table is built per game); the room's ingest decides.
            _ => self.forward_to_room(frame).await,
        }
    }

    async fn forward_to_room(&mut self, frame: FrameBody) {
        let mailbox = match &self.actions {
            Some(mb) => mb,
            None => {
                // Not in a room (or the room went away): report it.
                self.reply_err(ProtoError::NotInRoom).await;
                return;
            }
        };
        // The payload is forwarded encoded; the game crate decodes it.
        // Non-blocking: a flooding connection drops its own input (bounded
        // per-connection memory) and never stalls its actor or the room —
        // this `try_send` Full case is the architecture's only input-loss
        // point (the room's READ phase is a bounded pull that defers, not
        // drops), so the drop is counted here, attributed to this
        // connection's metrics sample. A closed channel means the room is
        // gone: detach.
        match mailbox.try_send(Action {
            conn: self.conn,
            // The connection actor cannot know the stable player identity
            // (it is minted inside the game logic): placeholder — the room
            // stamps the authoritative value from its binding table before
            // ingest (Faz 2).
            player: crate::id::PlayerId(0),
            op: frame.op,
            payload: frame.payload,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.m_actions_dropped += 1;
                if !self.m_actions_dropped_warned {
                    self.m_actions_dropped_warned = true;
                    warn!(
                        %self.conn,
                        op = frame.op,
                        "action channel full; this connection's input is being \
                         dropped (counted in its metrics sample; the room's \
                         per-connection per-tick pull budget bounds the \
                         damage — another connection's input is never \
                         affected)"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(%self.conn, "action channel closed while forwarding; detaching");
                self.detach();
            }
        }
    }

    /// Decode a frame body into a concrete message type via the table.
    fn decode<T: std::any::Any + Send + 'static>(
        &self,
        op: u16,
        frame: FrameBody,
    ) -> Result<T, ProtoError> {
        let decoded = self.table.decode(op, &frame.payload)?;
        decoded
            .downcast::<T>()
            .map(|b| *b)
            .map_err(|_| ProtoError::Other("type mismatch in message table".into()))
    }

    async fn send_frame<M: prost::Message + std::any::Any>(&mut self, op: u16, msg: &M) {
        if let Some(fb) = self.table.frame(op, msg) {
            // Metrics: count this control frame's wire bytes (frame body:
            // 2-byte op + payload). Room fan-out bytes are counted by the
            // room, not here.
            self.m_out_bytes =
                self.m_out_bytes.saturating_add(2 + fb.payload.len() as u64);
            self.m_out_frames += 1;
            let _ = self.out.send(vec![fb]).await;
        }
    }

    /// The single funnel for protocol errors: every `ERROR` frame this
    /// actor produces as a *response to a violation* passes through here,
    /// which classifies the error and hands it to [`Self::count_violation`]
    /// — where the violation budget lives (module docs).
    async fn reply_err(&mut self, e: ProtoError) {
        let (code, message) = match &e {
            ProtoError::UnknownOpcode(_) => (1, e.to_string()),
            ProtoError::Decode { .. } => (2, e.to_string()),
            ProtoError::NotAuthenticated => (3, e.to_string()),
            ProtoError::AlreadyAuthenticated => (3, e.to_string()),
            ProtoError::RoomNotFound(_) => (4, e.to_string()),
            ProtoError::NotInRoom => (6, e.to_string()),
            _ => (7, e.to_string()),
        };
        let class = violation_class(&e);
        self.count_violation(class, code, message).await;
    }

    /// The weighted-budget machinery shared by every counted violation:
    /// `reply_err` after classifying a decoded protocol error, and the
    /// §3.1 AUTH-attempt window directly (which raises a hard violation
    /// without a `ProtoError` to classify). Behaviour per violation:
    ///
    /// 1. counted violations add their weight to the lifetime score and
    ///    increment the event count (this also feeds the metrics delta);
    /// 2. if fewer than [`VIOLATION_ANSWER_LIMIT`] violations have been
    ///    answered so far, send the `ERROR` frame (the diagnosis);
    ///    otherwise stay silent (amplification is bounded here);
    /// 3. if the score just reached [`VIOLATION_BUDGET`], send the close
    ///    notice (`ERROR` code 9, the reason in the message — same code
    ///    family as idle timeout / connection capacity: a *server*
    ///    decision, the message carries the specificity), emit the
    ///    structured close signal with the peer address, and flag the run
    ///    loop to tear the connection down.
    async fn count_violation(&mut self, class: ViolationClass, code: u32, message: String) {
        let weight = class.weight();
        if weight == 0 {
            // Server-side condition: answered exactly as before the
            // budget existed, never counted (a client cannot fix the
            // registry being gone, and shutdown is transient).
            let _ = self
                .send_frame(op::base::ERROR, &base::Error { code, message })
                .await;
            return;
        }
        self.v_events += 1;
        self.v_score = self.v_score.saturating_add(weight);
        self.m_violations += 1;
        if self.v_answered < VIOLATION_ANSWER_LIMIT {
            self.v_answered += 1;
            debug!(
                %self.conn,
                %self.peer,
                code,
                ?class,
                score = self.v_score,
                "protocol violation answered (one of the first \
                 {VIOLATION_ANSWER_LIMIT}; later ones are silent)"
            );
            let _ = self
                .send_frame(op::base::ERROR, &base::Error { code, message })
                .await;
        }
        if self.v_score >= VIOLATION_BUDGET && !self.v_closing {
            self.v_closing = true;
            let reason = format!(
                "protocol violation budget exhausted: {} violations ({} \
                 answered) in this connection's lifetime",
                self.v_events, self.v_answered
            );
            // The close signal for a layer outside the server (firewall,
            // fail2ban, future auth): conn id, PEER ADDRESS, the violation
            // counts, and the reason — self-contained, structured, on the
            // tracing path every operator already collects.
            warn!(
                %self.conn,
                %self.peer,
                violations = self.v_events,
                answered = self.v_answered,
                score = self.v_score,
                "closing connection: protocol violation budget exhausted"
            );
            let _ = self
                .send_frame(op::base::ERROR, &base::Error { code: 9, message: reason })
                .await;
        }
    }

    /// The §3.3 pre-auth frame-budget close: an immediate `ERROR` code 9
    /// naming the policy (same server-decision family as the capacity and
    /// budget closes), then the ordinary teardown cascade via `p_closing`.
    async fn close_preauth_budget(&mut self) {
        self.p_closing = true;
        warn!(
            %self.conn,
            %self.peer,
            frames = self.preauth_frames,
            budget = PREAUTH_FRAME_BUDGET,
            "closing connection: pre-auth frame budget exhausted"
        );
        let _ = self
            .send_frame(
                op::base::ERROR,
                &base::Error {
                    code: 9,
                    message: format!(
                        "pre-auth frame budget exhausted: more than \
                         {PREAUTH_FRAME_BUDGET} frames received before \
                         authentication"
                    ),
                },
            )
            .await;
    }

    /// The single exit for ticket-validation failures (see `crate::auth`
    /// for the classification decision): an `ERROR` frame with code 10
    /// (ticket validation failed — rejected or timed out), the
    /// connection STAYS ALIVE, and the violation budget is NOT touched
    /// (a bad ticket is a normal rejection: the frame was well-formed
    /// and the client can fix its state; the budget is for structural
    /// protocol errors). The state machine is unchanged (still
    /// `WaitingAuth`): a fresh ticket may be re-presented.
    async fn reply_ticket_error(&mut self, e: crate::auth::TicketError) {
        warn!(%self.conn, %self.peer, reason = %e, "ticket validation failed (normal rejection; the connection stays alive)");
        let _ = self
            .send_frame(
                op::base::ERROR,
                &base::Error {
                    code: 10,
                    message: e.to_string(),
                },
            )
            .await;
    }
}
