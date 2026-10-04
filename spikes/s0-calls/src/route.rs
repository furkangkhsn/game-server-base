//! The service side's plumbing: what travels to a service (an
//! [`Envelope`]), how its answer travels back (a [`ReplySink`]), what a
//! client builds ([`CallSpec`]) and what a host registers ([`Served`]).

use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot};

use crate::contract::{
    ActorId, Call, CallCx, CallError, CallKey, Caller, Erased, Failure, Outcome, Refusal, fail_for,
};

/// A host-assigned service number (registration order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceId(pub u32);

/// Service-side loss counters ("count every loss": the two ways an answer
/// can fail to reach its caller once the service produced it).
#[derive(Debug, Default)]
pub struct ServiceStats {
    pub calls_received: AtomicU64,
    pub replies_undeliverable_full: AtomicU64,
    pub replies_undeliverable_gone: AtomicU64,
}

/// A clonable route to one service's mailbox.
pub struct Route<M> {
    pub(crate) id: ServiceId,
    pub(crate) name: &'static str,
    pub(crate) tx: mpsc::Sender<Envelope<M>>,
    pub(crate) stats: Arc<ServiceStats>,
}

impl<M> Clone for Route<M> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            name: self.name,
            tx: self.tx.clone(),
            stats: Arc::clone(&self.stats),
        }
    }
}

/// One call in a service's mailbox.
pub struct Envelope<M> {
    pub cx: CallCx,
    pub msg: M,
    pub(crate) sink: ReplySink,
}

/// What the tower stack sees: the context and the message.
pub struct Dispatch<M> {
    pub cx: CallCx,
    pub msg: M,
}

/// One finished call on its way back to a tick actor.
pub struct Completion {
    pub(crate) key: CallKey,
    pub(crate) outcome: Erased,
}

pub(crate) enum SinkTo {
    Actor(mpsc::Sender<Completion>),
    Await(oneshot::Sender<Erased>),
    Spent,
}

/// The typed-at-birth, erased-in-flight way back. Dropping it unanswered
/// (a panic, a stop, a bug) answers `Abandoned` at once.
pub struct ReplySink {
    pub(crate) key: CallKey,
    pub(crate) to: SinkTo,
    pub(crate) fail: fn(Failure) -> Erased,
    pub(crate) stats: Arc<ServiceStats>,
}

impl ReplySink {
    pub(crate) fn complete(mut self, outcome: Erased) {
        let to = std::mem::replace(&mut self.to, SinkTo::Spent);
        match to {
            SinkTo::Actor(tx) => match tx.try_send(Completion {
                key: self.key,
                outcome,
            }) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.stats
                        .replies_undeliverable_full
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.stats
                        .replies_undeliverable_gone
                        .fetch_add(1, Ordering::Relaxed);
                }
            },
            SinkTo::Await(tx) => {
                if tx.send(outcome).is_err() {
                    self.stats
                        .replies_undeliverable_gone
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            SinkTo::Spent => {}
        }
    }

    pub(crate) fn fail(self, f: Failure) {
        let out = (self.fail)(f);
        self.complete(out);
    }

    /// The call never left (refused at the mailbox): no answer is owed.
    pub(crate) fn disarm(mut self) {
        self.to = SinkTo::Spent;
    }
}

impl Drop for ReplySink {
    fn drop(&mut self) {
        if !matches!(self.to, SinkTo::Spent) {
            let out = (self.fail)(Failure::Abandoned);
            let sink = ReplySink {
                key: self.key,
                to: std::mem::replace(&mut self.to, SinkTo::Spent),
                fail: self.fail,
                stats: Arc::clone(&self.stats),
            };
            sink.complete(out);
        }
    }
}

/// Why a send to a mailbox failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendFail {
    Full,
    Gone,
}

pub(crate) type SendFn =
    Arc<dyn Fn(CallCx, ReplySink) -> Result<(), (SendFail, ReplySink)> + Send + Sync>;

/// A built call (from a generated client: `inv.craft(recipe, items)`),
/// not yet issued. Hand it to `calls.call(..)`.
pub struct CallSpec<C: Call> {
    pub(crate) service: ServiceId,
    pub(crate) name: &'static str,
    pub(crate) stats: Arc<ServiceStats>,
    pub(crate) send: SendFn,
    _c: PhantomData<fn() -> C>,
}

impl<C: Call> CallSpec<C> {
    #[doc(hidden)]
    pub fn new<M: Send + 'static>(route: &Route<M>, call: C, wrap: fn(C) -> M) -> Self {
        let tx = route.tx.clone();
        let send: SendFn = Arc::new(move |cx, sink| {
            match tx.try_send(Envelope {
                cx,
                msg: wrap(call.clone()),
                sink,
            }) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(env)) => Err((SendFail::Full, env.sink)),
                Err(mpsc::error::TrySendError::Closed(env)) => Err((SendFail::Gone, env.sink)),
            }
        });
        Self {
            service: route.id,
            name: route.name,
            stats: Arc::clone(&route.stats),
            send,
            _c: PhantomData,
        }
    }

    /// The service's name (spans, logs).
    pub fn service_name(&self) -> &'static str {
        self.name
    }
}

static HANDLE_SEQ: AtomicU64 = AtomicU64::new(1);
/// Handle calls are not in any room's record: their keys only need to be
/// unique in the process, not reproducible.
const HANDLE_ORIGIN: u64 = 0x4841_4e44_4c45_0000;

impl<M: Send + 'static> Route<M> {
    #[doc(hidden)]
    pub async fn call_await<C: Call>(
        &self,
        call: C,
        wrap: fn(C) -> M,
        player: Option<crate::contract::Identity>,
    ) -> Outcome<C> {
        let key = CallKey {
            origin: HANDLE_ORIGIN,
            seq: HANDLE_SEQ.fetch_add(1, Ordering::Relaxed),
        };
        let (tx, rx) = oneshot::channel();
        let sink = ReplySink {
            key,
            to: SinkTo::Await(tx),
            fail: fail_for::<C>,
            stats: Arc::clone(&self.stats),
        };
        let cx = CallCx {
            key,
            caller: Caller {
                actor: ActorId::HANDLE,
                player,
            },
        };
        match self.tx.try_send(Envelope {
            cx,
            msg: wrap(call),
            sink,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(env)) => {
                env.sink.disarm();
                return Err(CallError::Refused(Refusal::Full));
            }
            Err(mpsc::error::TrySendError::Closed(env)) => {
                env.sink.disarm();
                return Err(CallError::Refused(Refusal::Gone));
            }
        }
        // The one await: the sink always answers (Drop = Abandoned).
        match rx.await {
            Ok(erased) => match erased.downcast::<Outcome<C>>() {
                Ok(o) => *o,
                Err(_) => Err(CallError::Abandoned),
            },
            Err(_) => Err(CallError::Abandoned),
        }
    }
}

/// A service's identity: its name and mailbox message type.
pub trait ServiceKind: 'static {
    const NAME: &'static str;
    type Msg: Send + 'static;
}

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxFuture =
    Pin<Box<dyn std::future::Future<Output = Result<Erased, BoxError>> + Send + 'static>>;

/// "`S` implements this service": how to build its tower router.
pub trait ServiceDef<S>: ServiceKind {
    type Router: tower::Service<Dispatch<Self::Msg>, Response = Erased, Error = BoxError, Future = BoxFuture>
        + Clone
        + Send
        + Sync
        + 'static;
    fn router(svc: Arc<S>) -> Self::Router;
}

/// When a test-mode host delivers a service's answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delay {
    /// Visible in RESPONSES `n` ticks after the tick that issued the call
    /// (`n >= 1`; the default is 1: "next tick").
    Ticks(u64),
    /// Never answered (the sink is held until the host ends: `Abandoned`).
    Never,
}

impl Delay {
    /// Test mode: answers become visible `n` ticks after the call.
    pub fn ticks<K, S>(n: u64, served: Served<K, S>) -> Served<K, S> {
        Served {
            delay: Delay::Ticks(n.max(1)),
            ..served
        }
    }
    /// Test mode: never answers.
    pub fn never<K, S>(served: Served<K, S>) -> Served<K, S> {
        Served {
            delay: Delay::Never,
            ..served
        }
    }
}

/// An implementation ready for a host (`Inventory::serve(PgInventory::new(db))`).
pub struct Served<K, S> {
    pub(crate) svc: S,
    pub(crate) delay: Delay,
    _k: PhantomData<fn() -> K>,
}

impl<K, S> Served<K, S> {
    #[doc(hidden)]
    pub fn new(svc: S) -> Self {
        Self {
            svc,
            delay: Delay::Ticks(1),
            _k: PhantomData,
        }
    }
}

/// A generated client (`…Client` or `…Handle`).
pub trait ServiceClient: Sized {
    type Kind: ServiceKind;
    fn from_route(route: Route<<Self::Kind as ServiceKind>::Msg>) -> Self;
}

/// Anything that hands out routes: the live server, the test-mode host,
/// the replay host.
pub trait Host {
    fn route<K: ServiceKind>(&self) -> Route<K::Msg>;

    /// A generated client for a registered service.
    fn client<C: ServiceClient>(&self) -> C {
        C::from_route(self.route::<C::Kind>())
    }
}
