//! One client's RPC ledger: what it asked, what came back, and how each
//! answer is counted — the client half of the engine's "exactly one
//! answer per accepted request" (docs/RPC-CONTROL-PLANE.md §1, §3.1).
//!
//! The rules (each pinned by `tests.rs`):
//!
//! - ids are the client's own, `1, 2, 3, …` per session, never reused —
//!   so an answer names exactly one request, or none;
//! - the FIRST answer to a waiting request settles it: counted once, by
//!   its kind (`ok`, or the core's rejection it names), and its latency
//!   (send → arrival) recorded when it is `ok`;
//! - a second answer to a settled request is a DUPLICATE (`dup_answers`
//!   — must stay 0: F14's exactly-once), counted nowhere else;
//! - an answer to an id never sent (0 — the core's malformed answer —
//!   or past the last one) is UNMATCHED, counted nowhere else;
//! - the client's own timeout is the server's request timeout plus a
//!   margin (`limit`): an answer later than that still settles its
//!   request but is also `late`; a request still waiting at the end is
//!   `unanswered` when older than the limit (a client-side timeout),
//!   `open` otherwise (in flight when the run ended — not judged).

use std::time::{Duration, Instant};

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
    }
}

/// A sent request: still waiting (since when), or settled.
#[derive(Debug, Clone, Copy)]
enum Slot {
    Waiting(Instant),
    Settled,
}

/// One client's ledger (see the module docs).
pub(crate) struct Ledger {
    /// Request `id` is `slots[id - 1]`.
    slots: Vec<Slot>,
    limit: Duration,
    tally: RpcTally,
}

impl Ledger {
    /// An empty ledger whose client-side timeout is `limit`.
    pub(crate) fn new(limit: Duration) -> Self {
        Self {
            slots: Vec::new(),
            limit,
            tally: RpcTally::default(),
        }
    }

    /// Record a request sent at `at`; returns its id.
    pub(crate) fn sent(&mut self, at: Instant) -> u64 {
        self.slots.push(Slot::Waiting(at));
        self.tally.sent += 1;
        self.slots.len() as u64
    }

    /// Count one answer for `id` (`ok`, `reason`) that arrived at `at`.
    pub(crate) fn answer(&mut self, id: u64, ok: bool, reason: &str, at: Instant) {
        let slot = id
            .checked_sub(1)
            .and_then(|i| self.slots.get_mut(usize::try_from(i).ok()?));
        let Some(slot) = slot else {
            self.tally.unmatched += 1;
            return;
        };
        let Slot::Waiting(since) = *slot else {
            self.tally.dup_answers += 1;
            return;
        };
        *slot = Slot::Settled;
        let t = &mut self.tally;
        let waited = at.saturating_duration_since(since);
        if waited > self.limit {
            t.late += 1;
        }
        match classify(ok, reason) {
            Kind::Ok => {
                t.ok += 1;
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
    /// `unanswered` (older than the limit) or `open`.
    pub(crate) fn finish(mut self, now: Instant) -> RpcTally {
        for slot in &self.slots {
            if let Slot::Waiting(since) = *slot {
                if now.saturating_duration_since(since) > self.limit {
                    self.tally.unanswered += 1;
                } else {
                    self.tally.open += 1;
                }
            }
        }
        self.tally
    }
}

#[cfg(test)]
mod tests;
