//! A minimal in-process service host (round q1): a service is a bounded
//! mailbox ([`Route`]) drained by one task, each call answered by its own
//! spawned future. Enough to run the table end to end; q2's
//! `#[gsb::service]` tower host plugs into the same seam (a [`Route`],
//! [`Mail`] of [`Envelope`]s, [`Ask`](crate::calls::Ask)s inside the
//! message) and the same F5 stop ([`Service`]).

use std::future::Future;
use std::sync::atomic::Ordering;

use crate::calls::route::{Envelope, Mail, Route};
use crate::service::Service;

/// Serve a hand-written service: every call in its mailbox (`capacity`
/// calls) is handed to `handler`, whose future answers it (through the
/// [`Ask`](crate::calls::Ask)s in the message) in its own task. The
/// returned [`Service`] stops it (the stop-message idiom: queued calls
/// are served first). The task's one await is its mailbox's `recv`.
///
/// ```ignore
/// enum BankMsg { Transfer(Ask<Transfer>) }
/// let (bank, service) = calls::serve("bank", 64, |env: Envelope<BankMsg>| async move {
///     match env.msg { BankMsg::Transfer(ask) => ask.answer(Ok(TransferDone { .. })) }
/// });
/// ```
pub fn serve<M, F, Fut>(name: &'static str, capacity: usize, handler: F) -> (Route<M>, Service)
where
    M: Send + 'static,
    F: Fn(Envelope<M>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (route, mut rx) = Route::new(name, capacity);
    let stats = std::sync::Arc::clone(&route.stats);
    let task = tokio::spawn(async move {
        while let Some(mail) = rx.recv().await {
            match mail {
                Mail::Call(env) => {
                    stats.calls_received.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(handler(env));
                }
                Mail::Stop => break,
            }
        }
    });
    let stopper = route.clone();
    (route, Service::new(name, task, move || stopper.stop()))
}
