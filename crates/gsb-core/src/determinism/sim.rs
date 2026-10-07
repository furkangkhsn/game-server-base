//! TEST mode's services (`docs/SERVICES.md` §10.3): a fake service is an
//! ordinary handler over the [`Route`] seam — the one q1's
//! [`serve`](crate::calls::serve) and the `#[gsb::service]` host serve —
//! and the ENGINE owns its timing. No runtime: after each room tick the
//! test calls [`Sim::step`], which takes what the service mailboxes
//! hold, releases each call when its [`Delay`] says, and polls the
//! handler's future with a no-op waker. Same input, same ticks, same
//! answers — every run.
//!
//! ```ignore
//! let mut sim = Sim::new();
//! let bank = sim.service("bank", 64, Delay::ticks(3, |env: Envelope<BankMsg>| async move {
//!     match env.msg { BankMsg::Transfer(ask) => ask.answer(Ok(TransferDone { balance: 400 })) }
//! }));
//! // a call issued at tick T's FLUSH is answered at tick T + 3's RESPONSES
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::task::{Context, Waker};

use crate::calls::{Envelope, Mail, Route, ServiceId};
use crate::channel::Inbox;

/// A handler's future.
pub type HandlerFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What serves one call: the shape [`serve`](crate::calls::serve) takes
/// (`Fn(Envelope<M>) -> impl Future<Output = ()>`, answering through the
/// [`Ask`](crate::calls::Ask)s inside the message). Any such closure is
/// one; a generated service's served value can implement it too.
pub trait Handler<M>: Send + 'static {
    /// Serve one call.
    fn handle(&self, env: Envelope<M>) -> HandlerFuture;
}

impl<M, F, Fut> Handler<M> for F
where
    F: Fn(Envelope<M>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn handle(&self, env: Envelope<M>) -> HandlerFuture {
        Box::pin(self(env))
    }
}

/// When a fake service answers: wraps the served handler.
pub enum Delay<H> {
    /// A call issued at tick `T`'s FLUSH is served after tick
    /// `T + n - 1`, so its answer shows at tick `T + n`'s RESPONSES
    /// (`n = 1`: the next tick, as soon as a call can be answered; `0`
    /// counts as 1).
    Ticks(u64, H),
    /// Never served: the call is held until [`Sim::kill`] (which answers
    /// it `Abandoned`) — a service that hangs.
    Never(H),
}

impl<H> Delay<H> {
    /// Serve `handler` `n` ticks after the call.
    pub fn ticks(n: u64, handler: H) -> Self {
        Self::Ticks(n.max(1), handler)
    }
    /// Hold every call forever.
    pub fn never(handler: H) -> Self {
        Self::Never(handler)
    }
}

/// One call the sim holds: its tick, and its handler's future once
/// released.
struct Held<M> {
    issued: u64,
    env: Option<Envelope<M>>,
    fut: Option<HandlerFuture>,
}

trait SimService: Send {
    fn id(&self) -> ServiceId;
    fn step(&mut self, now: u64);
    fn kill(&mut self);
}

struct Svc<M, H> {
    id: ServiceId,
    stats: std::sync::Arc<crate::calls::ServiceStats>,
    inbox: Option<Inbox<Mail<M>>>,
    handler: H,
    /// `None`: never.
    delay: Option<u64>,
    held: Vec<Held<M>>,
}

impl<M: Send + 'static, H: Handler<M>> SimService for Svc<M, H> {
    fn id(&self) -> ServiceId {
        self.id
    }

    fn step(&mut self, now: u64) {
        if let Some(inbox) = &mut self.inbox {
            while let Ok(mail) = inbox.try_recv() {
                match mail {
                    Mail::Call(env) => {
                        self.stats.calls_received.fetch_add(1, Ordering::Relaxed);
                        self.held.push(Held {
                            issued: now,
                            env: Some(env),
                            fut: None,
                        });
                    }
                    Mail::Stop => {}
                }
            }
        }
        let Some(n) = self.delay else {
            return;
        };
        let mut cx = Context::from_waker(Waker::noop());
        let handler = &self.handler;
        self.held.retain_mut(|h| {
            if h.fut.is_none() {
                if now + 1 < h.issued + n {
                    return true;
                }
                if let Some(env) = h.env.take() {
                    h.fut = Some(handler.handle(env));
                }
            }
            match &mut h.fut {
                Some(f) => f.as_mut().poll(&mut cx).is_pending(),
                None => false,
            }
        });
    }

    fn kill(&mut self) {
        // The mailbox closes (later calls are refused `Gone`); every held
        // call drops its asks (answered `Abandoned`).
        self.inbox = None;
        self.held.clear();
    }
}

/// TEST mode's service host: fake services whose timing the test owns.
#[derive(Default)]
pub struct Sim {
    services: Vec<Box<dyn SimService>>,
}

impl Sim {
    /// An empty host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fake service (`mailbox`: its mailbox capacity) and get
    /// the route callers use — what [`serve`](crate::calls::serve) gives
    /// for a live one.
    pub fn service<M, H>(&mut self, name: &'static str, mailbox: usize, delay: Delay<H>) -> Route<M>
    where
        M: Send + 'static,
        H: Handler<M>,
    {
        let (route, inbox) = Route::new(name, mailbox.max(1));
        let (handler, delay) = match delay {
            Delay::Ticks(n, h) => (h, Some(n.max(1))),
            Delay::Never(h) => (h, None),
        };
        self.services.push(Box::new(Svc {
            id: route.id(),
            stats: std::sync::Arc::clone(route.stats()),
            inbox: Some(inbox),
            handler,
            delay,
            held: Vec::new(),
        }));
        route
    }

    /// Run the services after the room's tick `now` (the tick whose FLUSH
    /// sent the calls): take the mailboxes, serve what is due.
    pub fn step(&mut self, now: u64) {
        for s in &mut self.services {
            s.step(now);
        }
    }

    /// "The service died": its mailbox closes (later calls: `Gone`) and
    /// every call it held answers `Abandoned`.
    pub fn kill(&mut self, service: ServiceId) {
        for s in &mut self.services {
            if s.id() == service {
                s.kill();
            }
        }
    }
}
