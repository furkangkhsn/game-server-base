//! The RPC traffic mode (`--rpc-rate R [--rpc-burst B]`, BACKLOG B23):
//! beside its inputs, every client sends correlated requests — B at a
//! time, one burst every B / R seconds (R requests/s on average) — and
//! matches the answers riding its private frames (`Private.responses`)
//! against them in its [`Ledger`].
//!
//! The request is the demo's `ECONOMY` purchase of an item in its price
//! table: the external-I/O half of the RPC pattern (a pending slot, a
//! worker task, the economy service, a completion on a later tick), so
//! the per-connection pending cap, the worker completions and the
//! timeout sweep are all on the path. The demo is the only hosted game
//! with an RPC surface (the parser refuses the mode for the others).
//!
//! The schedule starts at the join (a request before it is not a room
//! action), phase-staggered by id so the clients do not burst in
//! lockstep. A missed slot is not made up (the rate is a ceiling): a
//! client that was away from its loop sends one burst and moves on.

use std::time::{Duration, Instant};

use bytes::Bytes;
use gsb_kit::client::wire::{Fields, Value};
use gsb_protocol::FrameBody;
use prost::Message;

mod ledger;
pub(crate) use ledger::*;

/// The run's RPC traffic: `rate` requests/s per client, sent `burst` at
/// a time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RpcPlan {
    pub(crate) rate: f64,
    pub(crate) burst: u32,
}

impl RpcPlan {
    /// The gap between two bursts: `burst / rate` seconds.
    pub(crate) fn interval(&self) -> Duration {
        Duration::from_secs_f64(f64::from(self.burst) / self.rate)
    }
}

/// How much longer than the server's request timeout a client waits
/// before it counts a request as timed out on its side: the server's
/// sweep answers at its deadline on the next tick, and the answer still
/// has to reach the client.
pub(crate) const LIMIT_MARGIN: Duration = Duration::from_secs(1);

/// The client-side timeout: the server's request timeout (the room
/// config default — the served server does not change it) plus
/// [`LIMIT_MARGIN`].
pub(crate) fn client_limit() -> Duration {
    gsb_core::room::RoomConfig::default().request_timeout + LIMIT_MARGIN
}

/// The item every request buys: in the demo's price table, so a healthy
/// request is answered `ok`.
pub(crate) const ITEM: &str = "potion";

/// The `ECONOMY` request body (the same for every request).
fn buy_payload() -> Bytes {
    gsb_demo::game::BuyItem { kind: ITEM.into() }
        .encode_to_vec()
        .into()
}

/// The request envelope for correlation id `id`.
fn request(id: u64, body: &Bytes) -> FrameBody {
    let env = gsb_protocol::base::RpcRequest {
        id,
        op: u32::from(gsb_demo::op::ECONOMY),
        payload: body.to_vec(),
    };
    FrameBody::new(gsb_protocol::op::base::RPC_REQ, env.encode_to_vec())
}

/// One client's RPC traffic: its schedule and its ledger.
pub(crate) struct RpcClient {
    plan: RpcPlan,
    /// When the first burst goes after the join (the id's phase).
    phase: Duration,
    /// The next burst (`None` until the join).
    next: Option<Instant>,
    body: Bytes,
    ledger: Ledger,
}

impl RpcClient {
    /// Client `id`'s traffic under `plan`.
    pub(crate) fn new(plan: RpcPlan, id: u64) -> Self {
        let interval = plan.interval();
        // The same id-based spread as the slow reader's phase.
        let phase = interval.mul_f64((id.wrapping_mul(997) % 1000) as f64 / 1000.0);
        Self {
            plan,
            phase,
            next: None,
            body: buy_payload(),
            ledger: Ledger::new(client_limit()),
        }
    }

    /// The join at `now`: the schedule starts.
    pub(crate) fn joined(&mut self, now: Instant) {
        self.next.get_or_insert(now + self.phase);
    }

    /// How long until the next burst (`None` before the join).
    pub(crate) fn until_due(&self, now: Instant) -> Option<Duration> {
        self.next.map(|t| t.saturating_duration_since(now))
    }

    /// The burst due at `now`, numbered and entered in the ledger — or
    /// nothing. The schedule moves one interval on, or past `now` when
    /// the client fell behind (no catch-up burst).
    pub(crate) fn due(&mut self, now: Instant) -> Option<Vec<FrameBody>> {
        let next = self.next.as_mut().filter(|t| **t <= now)?;
        let interval = self.plan.interval();
        *next += interval;
        if *next <= now {
            *next = now + interval;
        }
        let burst = (0..self.plan.burst)
            .map(|_| request(self.ledger.sent(now), &self.body))
            .collect();
        Some(burst)
    }

    /// Count the answers a private frame carries (arrived at `at`);
    /// returns how many it carried.
    pub(crate) fn on_private(&mut self, frame: &[u8], at: Instant) -> usize {
        let answers = responses(frame);
        for r in &answers {
            self.ledger.answer(r.id, r.ok, &r.reason, at);
        }
        answers.len()
    }

    /// The client's numbers, the ledger closed at `now`.
    pub(crate) fn finish(self, now: Instant) -> RpcTally {
        self.ledger.finish(now)
    }
}

/// The `Private.responses` field number — the same in every game's
/// private message (`repeated gsb.base.RpcResponse responses = 3`: the
/// kit's `Private` and each game's typed mirror of it).
const RESPONSES_FIELD: u32 = 3;

/// The RPC answers a private frame carries (an undecodable frame or
/// answer carries none: the view already counts the frame's error).
pub(crate) fn responses(frame: &[u8]) -> Vec<gsb_protocol::base::RpcResponse> {
    Fields::new(frame)
        .map_while(Result::ok)
        .filter_map(|(n, v)| match (n, v) {
            (RESPONSES_FIELD, Value::Len(body)) => {
                gsb_protocol::base::RpcResponse::decode(body).ok()
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests;
