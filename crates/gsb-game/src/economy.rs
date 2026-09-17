//! The in-process economy service: the demo's **reference adapter** for
//! delegated (external-I/O) RPC work.
//!
//! The RPC pattern (see `gsb_core::rpc`) requires a request whose answer
//! needs I/O the room cannot await (the room's tick body is synchronous
//! and its only await is the ticker). This module is the base's
//! in-process reference for "the platform's economy service": a channel
//! mailbox + one task, a request in, an oneshot answer out — the same
//! actor discipline as everything else (no shared state, no locks, one
//! bounded mailbox). A real deployment would replace the body with a
//! database call, a signature check, an HTTP round trip, …; the room's
//! `RequestDecision::External` future is the same either way (it is the
//! room that owns the correlation, the caps and the timeout — this
//! service is just the thing it delegates to).
//!
//! The service answers after a configurable `latency` (the simulated
//! I/O time; production adapters' latency is whatever the backend is).
//! The default (5 ms) is deliberately below the demo room's tick period
//! (33 ms at 30 Hz): the worker resolves inside one tick, and the
//! answer still arrives on the NEXT tick (the room drains its
//! completions channel in the CONTROL phase, before the tick that
//! registered the request ends) — which is exactly the client-visible
//! contract of the pattern ("the answer comes later, and looks like the
//! same kind of frame").

use tokio::sync::{mpsc, oneshot};

/// One purchase request to the economy service.
pub struct EconomyBuy {
    /// The item kind (demo vocabulary: see [`PRICES`]).
    pub kind: String,
    /// The answer: `Ok(price)` on success, `Err(reason)` on rejection
    /// (the room turns this into a normal rejection reply — the reason
    /// string is client-visible).
    pub reply: oneshot::Sender<Result<u32, String>>,
}

/// The demo price table (a wallet does not exist in the base; the price
/// is the observable effect the client sees in `BuyResult.price`).
pub const PRICES: &[(&str, u32)] = &[("potion", 100), ("sword", 500), ("shield", 300)];

/// The in-process economy service (see the module docs for the role).
#[derive(Debug)]
pub struct EconomyService {
    tx: mpsc::Sender<EconomyBuy>,
    /// The simulated I/O time per request (the "database latency").
    latency: std::time::Duration,
}

impl Clone for EconomyService {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            latency: self.latency,
        }
    }
}

impl EconomyService {
    /// Spawn the service task. `latency` is the simulated I/O time per
    /// request; `0` answers on the worker's next poll (tests). Returns
    /// a cloneable client handle (each room gets one).
    pub fn spawn(latency: std::time::Duration) -> Self {
        let (tx, mut rx) = mpsc::channel::<EconomyBuy>(64);
        tokio::spawn(async move {
            while let Some(buy) = rx.recv().await {
                let kind = buy.kind.clone();
                let reply = buy.reply;
                // The simulated I/O: an owning sleep (no room state is
                // held — the worker task owns this).
                let latency = latency;
                tokio::spawn(async move {
                    tokio::time::sleep(latency).await;
                    let price = PRICES.iter().find(|(k, _)| *k == kind).map(|(_, p)| *p);
                    let result = match price {
                        Some(p) => Ok(p),
                        None => Err(format!("unknown item `{kind}`")),
                    };
                    let _ = reply.send(result);
                });
            }
        });
        Self { tx, latency }
    }

    /// Default simulated I/O time (see the module docs: below one demo
    /// tick so the answer still arrives on the next tick).
    pub fn default_latency() -> std::time::Duration {
        std::time::Duration::from_millis(5)
    }

    /// Submit a purchase; the answer arrives on the returned future
    /// (an oneshot: one request, one answer; `Err` when the service is
    /// gone or dropped the reply — a server-side condition the room
    /// answers as a normal rejection).
    pub fn buy(&self, kind: String) -> impl std::future::Future<Output = Result<u32, String>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let tx = self.tx.clone();
        async move {
            if tx
                .send(EconomyBuy {
                    kind,
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                return Err("economy service gone".into());
            }
            match reply_rx.await {
                Ok(result) => result,
                Err(_) => Err("economy service dropped the reply".into()),
            }
        }
    }
}
