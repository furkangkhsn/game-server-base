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
//!   dropped channel, or the worker's timeout fires first);
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
use std::time::Instant;

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
    /// The rejection reason (empty when `ok`).
    pub reason: String,
    /// The response body (empty when `ok` is false).
    pub payload: Bytes,
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
        }
    }
}

/// An in-flight external request: the correlation id, the inner op
/// (echoed in the answer, so the deferred reply can be shaped like the
/// same-tick ones without re-reading the original request), and the
/// deadline after which the room's sweep answers it with a timeout.
#[derive(Debug)]
pub struct PendingRequest {
    pub id: u64,
    pub op: u16,
    pub due: Instant,
}

/// A worker report: the outcome of one delegated request, as resolved by
/// its worker task. The room reconciles it against its pending set (a
/// report for an id that is no longer pending — already answered,
/// timed out, or its connection left — is dropped).
#[derive(Debug)]
pub struct Completion {
    pub conn: ConnectionId,
    pub id: u64,
    pub ok: bool,
    /// The rejection reason (empty when `ok`).
    pub reason: String,
    /// The response body (empty when `ok` is false).
    pub payload: Bytes,
}

impl Completion {
    pub fn reply(conn: ConnectionId, id: u64, payload: Bytes) -> Self {
        Self {
            conn,
            id,
            ok: true,
            reason: String::new(),
            payload,
        }
    }

    pub fn error(conn: ConnectionId, id: u64, reason: String) -> Self {
        Self {
            conn,
            id,
            ok: false,
            reason,
            payload: Bytes::new(),
        }
    }
}

/// The client-visible reason string for the room's timeout sweep. Kept
/// in one place so the sweep and the tests agree on it.
pub const TIMEOUT_REASON: &str = "request timed out";
