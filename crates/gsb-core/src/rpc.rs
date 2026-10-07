//! Correlated requests (the RPC pattern): client → room → back to the
//! SAME connection, on the per-connection private frame path.
//!
//! A request arrives as an ordinary action carrying the base-band
//! envelope opcode [`gsb_protocol::op::base::RPC_REQ`]; the room's core
//! decodes the envelope (a base message) and hands the request to the
//! game logic, which decides what happens:
//!
//! - **room-local** ([`RequestDecision::Reply`]): the answer is computed
//!   in the tick that processes the request and delivered in that
//!   tick's broadcast phase (same-tick response);
//! - **rejected** ([`RequestDecision::Reject`]): a *normal* rejection
//!   (the request was not applied; the client sees the reason) — never
//!   a protocol-violation signal;
//! - **external I/O** ([`RequestDecision::External`]): the answer needs
//!   work the room cannot await (the tick body is synchronous, the room
//!   has exactly one await — the ticker). The logic returns an owning
//!   future; the room registers the request as *pending* and hands the
//!   future to a spawned **worker task**. The worker resolves it (or
//!   gives up at the room's request timeout) and reports back over a
//!   channel the room drains non-blockingly on a later tick's CONTROL
//!   phase; the room then delivers the answer through the same
//!   per-tick private path the local answers use. From the client's
//!   point of view both kinds look the same: one `Private.responses`
//!   entry per request, on whatever tick answered it.
//!
//! **Where the pending state lives** (the mechanism's core decision):
//! in the ROOM actor — the actor that owns the connection table, the
//! per-connection outbound channels, and the tick that delivers the
//! answer. The worker task owns only the delegated future; it carries
//! no state that outlives its single report. Consequences:
//!
//! - the room is the single authority on "is this request still
//!   pending", so exactly one answer per request is structural (a late
//!   report for an already-answered or timed-out id is dropped);
//! - per-connection pending caps and the room-level cap are enforced
//!   where the connection table already lives (no new shared state);
//! - a connection that leaves (or a room that shuts down) simply loses
//!   its pending set — late reports find no entry and are dropped, the
//!   workers exit on their own (their report send fails against the
//!   dropped channel, or the worker's timeout fires first); requests it
//!   sent that the room had not read yet (CONTROL runs before READ, so a
//!   leave can land in the same tick window) go with its action channel,
//!   neither processed nor answered, and are counted
//!   (`requests_dropped_unread`, B36);
//! - the tick body touches this machinery only through bounded
//!   non-blocking probes (an empty-channel `try_recv`, an
//!   `is_empty`-guarded sweep), so a quiet room pays nothing.
//!
//! **Correlation ids.** Client-assigned, per-connection, `u64`; `0` is
//! rejected (it cannot correlate). A duplicate id that is still PENDING
//! is rejected without re-processing (a retrying client must not buy a
//! double-applied side effect). A duplicate of an already-ANSWERED id
//! is processed: the room keeps no per-connection history beyond the
//! pending set (an unbounded "seen ids" table would be a leak), and
//! from that point a recycled number is indistinguishable from a fresh
//! request. Duplicates/stale ids are normal rejections, never counted
//! against the connection's protocol-violation budget: the id space is
//! not a security boundary (the request is already scoped to this
//! connection's stream) and a client re-sending its own request within
//! one RTT is always legitimate.
//!
//! **Timeouts.** The room's request timeout bounds the wait the client
//! can observe: a pending request whose deadline passed is swept on the
//! next tick's CONTROL phase and answered with a *timeout* reply (the
//! client never hangs on a request the server accepted — see
//! "Delivery" for what that takes of the connection). The worker
//! task carries the same timeout internally as a *resource* guard: a
//! future the game logic never resolves cannot outlive
//! `timeout + ε` (without it, a stuck external call would leak a task
//! per request — an unbounded resource-consumption path). The room's
//! sweep is the client-visible authority; the worker's timeout only
//! bounds the task's lifetime and reports nothing on expiry.
//!
//! **Ordering within a tick.** The tick processes all fire-and-forget
//! actions first (the logic's `ingest`, arrival order preserved), then
//! the requests (arrival order preserved). A request therefore sees the
//! world AFTER this tick's actions were applied — the common case for
//! "validate what I just sent". Strict action/request interleaving is
//! not provided (it would require the logic to process a mixed
//! sequence; the two kinds have different latency by design, so no
//! game-correct ordering is lost by the split).
//!
//! **Capacity.** The pending set is bounded twice: per connection
//! (`RoomConfig::max_pending_requests_per_conn`) and room-wide
//! (`RoomConfig::max_pending_requests`). A request arriving past a cap
//! is answered with a normal rejection in the same tick. The rate at
//! which requests can arrive is already bounded by the READ phase's
//! per-connection pull budget (a request IS an action, pulled under the
//! same fairness cut), so a flooding connection cannot buy more
//! pending state than its budget allows per tick.
//!
//! **Delivery.** An answer rides the connection's private frame in the
//! fan-out batch, and a batch its bounded outbound channel refuses is
//! dropped whole. The answers it carried are the core's own: they go
//! back to the FRONT of the connection's queue and are handed to the
//! logic's `private` again on the next tick, ahead of any later answer,
//! until a batch carrying them is accepted — exactly once, in order
//! (they leave the queue only when taken, return only on a drop). So an
//! accepted request is answered on the connection as soon as the
//! connection drains; only a session that ends first (leave, a detach
//! — including the write-stall close of a client that never drains —
//! or a migration to another shard) takes its undelivered answers
//! along, like its in-flight requests, and leaves the client to its
//! own timeout.
//!
//! The storm bound: while a connection is congested (its latest batch
//! was dropped), a request is accepted only if the connection owes
//! fewer answers — queued, carried, and in flight together — than
//! `RoomConfig::max_pending_requests_per_conn`. A request past that is
//! REFUSED: neither processed nor answered (any answer, a rejection
//! included, would be one more undelivered answer), counted on its own
//! (`requests_refused_congested`, apart from the answered cap
//! rejections — F15); nothing was applied, so the client's
//! retry after its own timeout is safe. While congested, what a
//! connection owes never grows past the larger of the cap and what it
//! owed when the run of drops began — and that is at most the in-flight
//! cap plus the answers of the one tick whose batch dropped first:
//! `max_pending_requests_per_conn + max_actions_per_conn_per_tick`
//! (4 + 16 = 20 by default; a request is an action, pulled under the
//! same per-connection budget). A connection whose batches go through
//! is never refused.

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;

use crate::id::{ConnectionId, PlayerId};

/// The opcode of the request envelope (the base band — the room's core
/// can decode it; the game crate owns the INNER opcodes).
pub const RPC_REQ_OP: u16 = gsb_protocol::op::base::RPC_REQ;

/// A correlated request: the decoded base envelope plus the owner
/// connection. The inner `op` + `payload` are game-encoded and opaque
/// to the core (the logic decodes them against its own message types).
#[derive(Debug)]
pub struct RpcRequest {
    /// The transport session that sent the request (the answer goes back
    /// to it, and only to it; pending/reply state is keyed by it —
    /// session-scoped by design).
    pub conn: ConnectionId,
    /// The stable player identity the room resolved for this session at
    /// ingest (Faz 2): what the LOGIC keys its lookups with. A resumed
    /// player's requests resolve to the same player across sessions.
    pub player: PlayerId,
    /// The client's correlation id (see the module docs for the id-space
    /// rules; `0` is rejected by the room).
    pub id: u64,
    /// The inner (game-band) opcode the request refers to.
    pub op: u16,
    /// The inner (game-band) payload, still encoded.
    pub payload: Bytes,
}

/// The game logic's decision for one request (see the module docs).
///
/// `External` carries an **owning** future (`'static`): it must not
/// borrow the room, its world, or the logic (the tick body that spawned
/// it ends long before the future resolves). The logic captures by value
/// whatever the work needs (a channel sender, a config value, …).
pub enum RequestDecision {
    /// Room-local: this is the response body (the encoded message of the
    /// response kind paired with the request's inner op). Answered in
    /// the same tick.
    Reply(Bytes),
    /// Rejected, room-local: a normal rejection with a
    /// client-actionable reason (never a violation signal). Answered in
    /// the same tick.
    Reject(String),
    /// External I/O: the work is delegated; the room registers the
    /// request as pending and the worker resolves the future (bounded by
    /// the room's request timeout). The answer arrives on a later tick.
    External(Pin<Box<dyn Future<Output = Result<Bytes, String>> + Send + 'static>>),
}

/// A queued response waiting for this tick's broadcast phase (delivered
/// through the logic's `private` seam as a `Private.responses` entry).
#[derive(Debug)]
pub struct RpcReply {
    /// The correlation id the reply belongs to (echoes the request).
    pub id: u64,
    /// `true`: `payload` carries the response body; `false`: `reason`
    /// carries the rejection reason.
    pub ok: bool,
    /// The inner op the reply answers (echoes the request's inner op).
    pub op: u16,
    /// The rejection reason (empty when `ok`, and for a typed game
    /// error).
    pub reason: String,
    /// The response body when `ok`; the typed game error when
    /// `game_error`; empty otherwise.
    pub payload: Bytes,
    /// `ok == false` and `payload` is the request's TYPED game error
    /// (`Request::Error`, `RpcResponse.game_error` — round q3,
    /// `docs/SERVICES.md` §16). `false` on every engine answer.
    pub game_error: bool,
}

impl RpcReply {
    /// A successful answer: `payload` is the response body.
    pub fn ok(id: u64, op: u16, payload: Bytes) -> Self {
        Self {
            id,
            ok: true,
            op,
            reason: String::new(),
            payload,
            game_error: false,
        }
    }

    /// An engine (or legacy `Reject`) answer: the text is in `reason`.
    pub fn rejected(id: u64, op: u16, reason: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            op,
            reason: reason.into(),
            payload: Bytes::new(),
            game_error: false,
        }
    }

    /// The request's typed game error: `payload` is the encoded
    /// `Request::Error` (possibly zero bytes), `reason` stays empty.
    pub fn game_error(id: u64, op: u16, payload: Bytes) -> Self {
        Self {
            id,
            ok: false,
            op,
            reason: String::new(),
            payload,
            game_error: true,
        }
    }
}

impl From<&RpcReply> for gsb_protocol::base::RpcResponse {
    /// The reply's wire shape. Kept HERE, next to the request decoding,
    /// because the core owns the whole correlated-request envelope —
    /// both halves now live in `base.proto`. Without it every game crate
    /// re-writes this mapping (and the `u16 -> u32` op widening the
    /// proto3 type system forces) by hand, which is the per-game
    /// duplication the envelope's move to the base protocol removes.
    ///
    /// Takes `&RpcReply`: the room hands the logic a *slice* of queued
    /// replies for the tick and keeps ownership (a reply may be shaped
    /// into more than one encoder — see the AOI one-shot path).
    fn from(r: &RpcReply) -> Self {
        Self {
            id: r.id,
            ok: r.ok,
            // The op space is `u16` by protocol contract; proto3 has no
            // 16-bit integer, so the wire field is `uint32` and this
            // widening is lossless and total.
            op: u32::from(r.op),
            reason: r.reason.clone(),
            payload: r.payload.to_vec(),
            game_error: r.game_error,
        }
    }
}

/// The client-visible reason string for the room's timeout sweep. Kept
/// in one place so the sweep and the tests agree on it.
pub const TIMEOUT_REASON: &str = "request timed out";

/// The client-visible reason of a request refused at the per-connection
/// pending cap (`RoomConfig::max_pending_requests_per_conn`). The core's
/// rejection reasons live here, next to [`TIMEOUT_REASON`], so the room,
/// the shard and a client that tells the rejections apart (the load
/// generator) read one spelling.
pub const CONN_CAP_REASON: &str = "pending request limit reached (per connection)";

/// The client-visible reason of a request refused at the room-wide
/// pending cap (`RoomConfig::max_pending_requests`).
pub const ROOM_CAP_REASON: &str = "pending request limit reached (room)";

/// The client-visible reason of a request whose id is still in flight
/// on its connection (answered without re-processing).
pub const DUPLICATE_REASON: &str = "duplicate request id (the request is still in flight)";

/// The client-visible reason of an undecodable envelope or a correlation
/// id of 0 (answered under id 0 — it cannot correlate).
pub const MALFORMED_REASON: &str = "malformed request envelope (or correlation id = 0)";

/// The client-visible reason of a request no handler takes: this prefix,
/// then the inner op (`{:#04x}`) — see [`no_handler_reason`].
pub const NO_HANDLER_PREFIX: &str = "no request handler for op";

/// The "no handler" reason for inner op `op`.
pub fn no_handler_reason(op: u16) -> String {
    format!("{NO_HANDLER_PREFIX} {op:#04x}")
}

/// The client-visible reasons of the call table's engine answers
/// (`crate::calls`, SERVICES.md §5.4): a request of a registered op
/// whose payload did not decode as its type.
pub const UNDECODABLE_REASON: &str = "undecodable request payload";

/// A request from a session bound to no owner (the game never sees it).
pub const NOT_BOUND_REASON: &str = "session not bound to an owner";

/// A request nobody took (`take_incoming`) by the end of its tick.
pub const UNTAKEN_REASON: &str = "request not handled this tick";

/// A request taken but neither answered nor attached to a call by the end
/// of its tick.
pub const UNANSWERED_REASON: &str = "request taken but not answered";

/// An `.answers` call the engine refused: the service's mailbox was full.
pub const REFUSED_FULL_REASON: &str = "service busy";

/// An `.answers` call the engine refused: the service was gone.
pub const REFUSED_GONE_REASON: &str = "service unavailable";

/// An `.answers` call the engine refused: the caller's share of the
/// service was used up.
pub const REFUSED_SHARE_REASON: &str = "service share exhausted";

/// An `.answers` call the service's load shedding refused.
pub const REFUSED_SHED_REASON: &str = "service overloaded";

/// An `.answers` call the service dropped unanswered.
pub const ABANDONED_REASON: &str = "service dropped the request";

#[cfg(test)]
mod tests {
    use super::*;

    /// The rejection reasons are client-visible text (a client may tell
    /// them apart — the load generator's RPC mode does): pinned byte for
    /// byte, so naming them changed no answer and a new spelling is a
    /// decision, not an accident.
    #[test]
    fn the_rejection_reasons_are_pinned() {
        assert_eq!(TIMEOUT_REASON, "request timed out");
        assert_eq!(
            CONN_CAP_REASON,
            "pending request limit reached (per connection)"
        );
        assert_eq!(ROOM_CAP_REASON, "pending request limit reached (room)");
        assert_eq!(
            DUPLICATE_REASON,
            "duplicate request id (the request is still in flight)"
        );
        assert_eq!(
            MALFORMED_REASON,
            "malformed request envelope (or correlation id = 0)"
        );
        assert_eq!(no_handler_reason(0x3ee), "no request handler for op 0x3ee");
        assert_eq!(no_handler_reason(7), "no request handler for op 0x07");
        assert_eq!(UNDECODABLE_REASON, "undecodable request payload");
        assert_eq!(NOT_BOUND_REASON, "session not bound to an owner");
        assert_eq!(UNTAKEN_REASON, "request not handled this tick");
        assert_eq!(UNANSWERED_REASON, "request taken but not answered");
        assert_eq!(REFUSED_FULL_REASON, "service busy");
        assert_eq!(REFUSED_GONE_REASON, "service unavailable");
        assert_eq!(REFUSED_SHARE_REASON, "service share exhausted");
        assert_eq!(REFUSED_SHED_REASON, "service overloaded");
        assert_eq!(ABANDONED_REASON, "service dropped the request");
    }
}
