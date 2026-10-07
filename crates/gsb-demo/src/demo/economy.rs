//! The in-process economy service: the demo's **reference adapter** for
//! delegated (external-I/O) RPC work, written as a `#[gsb_core::service]`
//! trait (`docs/SERVICES.md` §4.1, Faz 2 — round q2).
//!
//! The RPC pattern (see `gsb_core::rpc`) requires a request whose answer
//! needs I/O the room cannot await (the room's tick body is synchronous
//! and its only await is the ticker). This module is the base's
//! in-process reference for "the platform's economy service": the
//! [`Economy`] trait (one method, `buy`), its demo implementation
//! [`PriceList`], hosted on the engine's tower host — a bounded mailbox,
//! one task, each call answered from its own task. A real deployment
//! would replace the body with a database call, a signature check, an
//! HTTP round trip, …; the room's `RequestDecision::External` future is
//! the same either way (it is the room that owns the correlation, the
//! caps and the client-visible timeout — this service is just the thing
//! it delegates to).
//!
//! **Why the rooms keep `External`.** The demo's `ECONOMY` wire answers
//! a refusal with `ok = false` and the reason in `reason` — the bytes
//! `gsb-demo`'s wire contract locks. The call table's `.answers(..)` path
//! puts a typed service error in `payload` with an empty `reason` (until
//! Faz 3's `game_error` flag), which would change those bytes. So the
//! room's future awaits the awaitable client ([`EconomyHandle`]) and maps
//! every ending onto the same reason strings as before.
//!
//! The service answers after a configurable `latency` (the simulated
//! I/O time; production adapters' latency is whatever the backend is).
//! The default (5 ms) is deliberately below the demo room's tick period
//! (33 ms at 30 Hz): the worker resolves inside one tick, and the
//! answer still arrives on the NEXT tick — which is exactly the
//! client-visible contract of the pattern ("the answer comes later, and
//! looks like the same kind of frame").
//!
//! **Stop (BACKLOG F5).** A game module hosts it with
//! `parts.service(Economy::serve(PriceList::new(latency)), |s| s.mailbox(64))`
//! (`gsb-server`'s demo module); the server asks it to stop only after
//! every room ended. [`EconomyService::start`] does the same outside a
//! server and returns the [`Service`]. On the stop the host serves what
//! was queued ahead of it, refuses what comes after ("economy service
//! gone"), waits for the answers it already owes, and ends.
//! [`EconomyService::spawn`] keeps the pre-F5 life: no stop request, the
//! task ends when its last client drops.

use std::time::Duration;

use gsb_core::calls::{CallCx, CallError, Refusal, ServiceClient};
use gsb_core::service::Service;

/// The demo price table (a wallet does not exist in the base; the price
/// is the observable effect the client sees in `BuyResult.price`).
pub const PRICES: &[(&str, u32)] = &[("potion", 100), ("sword", 500), ("shield", 300)];

/// The economy service's contract.
#[gsb_core::service]
pub trait Economy {
    /// Buy one item of `kind`: its price, or the client-visible reason
    /// it was refused.
    async fn buy(&self, cx: &CallCx, kind: String) -> Result<u32, String>;
}

/// The demo's economy: [`PRICES`], answered after a simulated I/O time.
#[derive(Debug, Clone, Copy)]
pub struct PriceList {
    latency: Duration,
}

impl PriceList {
    /// A price list answering after `latency` (`0`: on the call task's
    /// first poll).
    pub fn new(latency: Duration) -> Self {
        Self { latency }
    }
}

impl Economy for PriceList {
    async fn buy(&self, _cx: &CallCx, kind: String) -> Result<u32, String> {
        // The simulated I/O: the call's own task sleeps (no room state is
        // held — the host owns this task).
        tokio::time::sleep(self.latency).await;
        match PRICES.iter().find(|(k, _)| *k == kind) {
            Some((_, p)) => Ok(*p),
            None => Err(format!("unknown item `{kind}`")),
        }
    }
}

/// The room-side handle on the economy (each room gets a clone): the
/// awaitable client, with every ending mapped onto a client-visible
/// reason (see the module docs).
#[derive(Clone)]
pub struct EconomyService {
    handle: EconomyHandle,
}

impl std::fmt::Debug for EconomyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EconomyService").finish_non_exhaustive()
    }
}

/// The demo's mailbox size (as before the tower host).
const MAILBOX: usize = 64;

impl EconomyService {
    /// Spawn the service (a [`PriceList`] with `latency`). Returns a
    /// cloneable client handle (each room gets one). The task ends when
    /// the last handle drops; [`Self::start`] adds an explicit stop.
    pub fn spawn(latency: Duration) -> Self {
        Self::start(latency).0
    }

    /// Spawn the service, returning the client handle AND the service's
    /// [`Service`] (named `Economy`) for an explicit stop (see the
    /// module docs). Dropping the `Service` leaves the [`Self::spawn`]
    /// life.
    pub fn start(latency: Duration) -> (Self, Service) {
        let (route, service) =
            gsb_core::calls::spawn(Economy::serve(PriceList::new(latency)), |s| {
                s.mailbox(MAILBOX)
            });
        (Self::from_handle(EconomyHandle::from_route(route)), service)
    }

    /// Wrap the awaitable client of an economy hosted elsewhere (a
    /// server's `parts.client::<EconomyHandle>()`).
    pub fn from_handle(handle: EconomyHandle) -> Self {
        Self { handle }
    }

    /// Default simulated I/O time (see the module docs: below one demo
    /// tick so the answer still arrives on the next tick).
    pub fn default_latency() -> Duration {
        Duration::from_millis(5)
    }

    /// The demo's mailbox size, for a module hosting [`PriceList`] itself.
    pub fn mailbox() -> usize {
        MAILBOX
    }

    /// Submit a purchase; the answer arrives on the returned future
    /// (`Err` carries the client-visible reason: the service's own, or
    /// a server-side condition the room answers as a normal rejection).
    pub fn buy(&self, kind: String) -> impl std::future::Future<Output = Result<u32, String>> {
        let handle = self.handle.clone();
        async move {
            handle.buy(kind).await.map_err(|e| match e {
                CallError::Service(reason) => reason,
                CallError::Refused(Refusal::Gone) => "economy service gone".into(),
                CallError::Refused(_) => "economy service busy".into(),
                CallError::TimedOut => "economy service timed out".into(),
                CallError::Abandoned => "economy service dropped the reply".into(),
            })
        }
    }
}

#[cfg(test)]
mod tests;
