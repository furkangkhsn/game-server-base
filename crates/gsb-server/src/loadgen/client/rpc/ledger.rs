//! One client's RPC ledger: what it asked, what came back, and how each
//! answer is counted — the client half of the engine's "exactly one
//! answer per accepted request" (docs/RPC-CONTROL-PLANE.md §1, §3.1).
//!
//! The rules (each pinned by `tests.rs`):
//!
//! - ids are the typed request table's (`gsb_client::requests`, B161),
//!   `1, 2, 3, …` per session, never reused — so an answer names exactly
//!   one request, or none;
//! - the FIRST answer to a waiting request settles it: counted once, by
//!   its kind (`ok` — its payload decoded as `BuyResult` —, the core's
//!   rejection it names, or `undecodable`), and its latency (send →
//!   arrival) recorded when it is `ok`;
//! - a second answer to a settled request is a DUPLICATE (`dup_answers`
//!   — must stay 0: F14's exactly-once), counted nowhere else;
//! - an answer to an id never sent (0 — the core's malformed answer —
//!   or past the last one) is UNMATCHED, counted nowhere else;
//! - the client's own timeout is the server's request timeout plus a
//!   margin (`limit`), on the table's clock: a request is timed out once
//!   its limit is reached; an answer after that is `late` (and, since
//!   B161, counted under no kind — the table already ended the request);
//!   a request timed out and never answered is `unanswered`; one still in
//!   flight at the end is `open` (not judged).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gsb_client::requests::{RequestError, Requests, Ticket};
use gsb_protocol::FrameBody;
use gsb_protocol::base::RpcResponse;

/// What one answer says, as the client can tell it: `ok`, or the core's
/// rejection its reason names (`gsb_core::rpc`'s reason constants —
/// the spelling the room and the shard answer with), or the game's own
/// rejection (`Logic`: any other reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Ok,
    Timeout,
    ConnCap,
    RoomCap,
    Dup,
    NoHandler,
    Malformed,
    Logic,
}

/// The kind of an answer (`ok`, `reason`).
pub(crate) fn classify(ok: bool, reason: &str) -> Kind {
    use gsb_core::rpc as core;
    match reason {
        _ if ok => Kind::Ok,
        core::TIMEOUT_REASON => Kind::Timeout,
        core::CONN_CAP_REASON => Kind::ConnCap,
        core::ROOM_CAP_REASON => Kind::RoomCap,
        core::DUPLICATE_REASON => Kind::Dup,
        core::MALFORMED_REASON => Kind::Malformed,
        r if r.starts_with(core::NO_HANDLER_PREFIX) => Kind::NoHandler,
        _ => Kind::Logic,
    }
}

/// One client's RPC numbers (summed over the clients by the report).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RpcTally {
    /// Requests sent.
    pub(crate) sent: u64,
    /// First answers, by kind.
    pub(crate) ok: u64,
    pub(crate) timeout: u64,
    pub(crate) conn_cap: u64,
    pub(crate) room_cap: u64,
    pub(crate) dup: u64,
    pub(crate) no_handler: u64,
    pub(crate) malformed: u64,
    pub(crate) logic: u64,
    /// Second (or later) answers to a settled request (must be 0).
    pub(crate) dup_answers: u64,
    /// Answers to an id this client never sent.
    pub(crate) unmatched: u64,
    /// First answers that came after the client's limit.
    pub(crate) late: u64,
    /// Requests never answered and older than the limit at the end.
    pub(crate) unanswered: u64,
    /// Requests never answered and younger than the limit at the end.
    pub(crate) open: u64,
    /// Every `ok` answer's latency, µs (send → arrival).
    pub(crate) ok_lat_us: Vec<u32>,
    /// Answers whose payload did not decode as the request's declared
    /// type (`BuyResult`; the typed client's `undecodable`).
    pub(crate) undecodable: u64,
}

impl RpcTally {
    /// First answers of every kind.
    pub(crate) fn answered(&self) -> u64 {
        self.ok
            + self.timeout
            + self.conn_cap
            + self.room_cap
            + self.dup
            + self.no_handler
            + self.malformed
            + self.logic
    }

    /// Requests the client timed out on: no answer within the limit
    /// (never answered, or answered late).
    pub(crate) fn client_timeouts(&self) -> u64 {
        self.unanswered + self.late
    }

    /// Add `o` (the report's sum over the clients).
    pub(crate) fn add(&mut self, o: &RpcTally) {
        self.sent += o.sent;
        self.ok += o.ok;
        self.timeout += o.timeout;
        self.conn_cap += o.conn_cap;
        self.room_cap += o.room_cap;
        self.dup += o.dup;
        self.no_handler += o.no_handler;
        self.malformed += o.malformed;
        self.logic += o.logic;
        self.dup_answers += o.dup_answers;
        self.unmatched += o.unmatched;
        self.late += o.late;
        self.unanswered += o.unanswered;
        self.open += o.open;
        self.ok_lat_us.extend_from_slice(&o.ok_lat_us);
        self.undecodable += o.undecodable;
    }
}

/// The request every client sends (the demo's `Buy`, op 1006).
pub(crate) type Buy = gsb_demo::game::BuyItem;

/// One client's ledger (see the module docs) over the typed request
/// table, `gsb_client::requests::Requests` (B161): it numbers the
/// requests, builds their frames, matches and decodes the answers and
/// times the requests out at the client's limit; the ledger keeps what
/// it adds — the send times (latency), the timed-out ids (a late answer
/// is told from a duplicate or an unmatched one) and the tally.
pub(crate) struct Ledger {
    requests: Requests,
    /// In flight: each request's ticket and send time.
    open: HashMap<u64, (Ticket<Buy>, Instant)>,
    /// Timed out on the client's clock and not answered since.
    timed_out: HashSet<u64>,
    /// The last id the table minted (ids are `1..=last`).
    last: u64,
    tally: RpcTally,
}

impl Ledger {
    /// An empty ledger whose client-side timeout is `limit`.
    pub(crate) fn new(limit: Duration) -> Self {
        Self {
            requests: Requests::new(limit),
            open: HashMap::new(),
            timed_out: HashSet::new(),
            last: 0,
            tally: RpcTally::default(),
        }
    }

    /// Start request `body` at `at`: its `RPC_REQ` frame (its id is the
    /// table's next, from 1).
    pub(crate) fn start(&mut self, body: &Buy, at: Instant) -> FrameBody {
        let (ticket, frame) = self.requests.start(body, at);
        self.last = ticket.id();
        self.open.insert(ticket.id(), (ticket, at));
        self.tally.sent += 1;
        frame
    }

    /// The requests the table timed out by `now` leave `open`.
    fn sweep(&mut self, now: Instant) {
        self.requests.sweep(now);
        let Self {
            requests,
            open,
            timed_out,
            ..
        } = self;
        open.retain(|id, (ticket, _)| match requests.take(ticket) {
            Some(_) => {
                timed_out.insert(*id);
                false
            }
            None => true,
        });
    }

    /// Count one answer `r` that arrived at `at`.
    pub(crate) fn answer(&mut self, r: RpcResponse, at: Instant) {
        self.sweep(at);
        let id = r.id;
        let Some((ticket, since)) = self.open.remove(&id) else {
            // Not in flight: the table drops it (its `answers_late`);
            // the ledger says which kind of stray it is.
            self.requests.answer(r);
            if self.timed_out.remove(&id) {
                self.tally.late += 1;
            } else if (1..=self.last).contains(&id) {
                self.tally.dup_answers += 1;
            } else {
                self.tally.unmatched += 1;
            }
            return;
        };
        self.requests.answer(r);
        let t = &mut self.tally;
        let Some(outcome) = self.requests.take(&ticket) else {
            return;
        };
        let kind = match outcome {
            Ok(_) => Kind::Ok,
            Err(RequestError::Engine(reason)) => classify(false, &reason),
            Err(RequestError::Game(_)) => Kind::Logic,
            Err(RequestError::Undecodable) => {
                t.undecodable += 1;
                return;
            }
            // Taken right after its answer: none of these can be it.
            Err(RequestError::TimedOut | RequestError::SessionEnded | RequestError::Forgotten) => {
                return;
            }
        };
        match kind {
            Kind::Ok => {
                t.ok += 1;
                let waited = at.saturating_duration_since(since);
                t.ok_lat_us
                    .push(u32::try_from(waited.as_micros()).unwrap_or(u32::MAX));
            }
            Kind::Timeout => t.timeout += 1,
            Kind::ConnCap => t.conn_cap += 1,
            Kind::RoomCap => t.room_cap += 1,
            Kind::Dup => t.dup += 1,
            Kind::NoHandler => t.no_handler += 1,
            Kind::Malformed => t.malformed += 1,
            Kind::Logic => t.logic += 1,
        }
    }

    /// Close the ledger at `now`: every request still waiting is
    /// `unanswered` (timed out on the client's clock) or `open`.
    pub(crate) fn finish(mut self, now: Instant) -> RpcTally {
        self.sweep(now);
        self.tally.unanswered = self.timed_out.len() as u64;
        self.tally.open = self.open.len() as u64;
        debug_assert_eq!(
            self.requests.counters().timed_out,
            self.tally.client_timeouts(),
            "the table's timeouts are the late and the unanswered"
        );
        self.tally
    }
}

#[cfg(test)]
mod tests;
