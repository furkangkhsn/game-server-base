//! The service side's plumbing — the seam a service host plugs into
//! (this round's minimal [`serve`](crate::calls::serve), q2's
//! `#[gsb::service]` tower host):
//!
//! - a [`Route`] is a clonable handle on one service's bounded mailbox;
//! - what travels in it is [`Mail::Call`] of an [`Envelope`] (the
//!   [`CallCx`] and the service's own message `M`);
//! - each call inside `M` is an [`Ask<C>`]: the call's arguments and the
//!   typed right to answer it ([`Ask::answer`]). Dropping it unanswered
//!   answers [`CallError::Abandoned`] at once — a panicking or stopping
//!   service never leaves a caller waiting;
//! - a [`CallSpec<C>`] is a built call, not yet issued: hand it to
//!   [`Calls::call`](crate::calls::Calls::call).
//!
//! The answer travels back over the calling table's own bounded
//! completion channel with a `try_send`: a full channel loses the answer
//! and counts it at the service (`ServiceStats::replies_undeliverable_full`),
//! a gone caller likewise (`…_gone`). No await on the caller's side,
//! ever.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot};

use crate::calls::contract::{
    ActorId, Call, CallCx, CallError, CallKey, Caller, Erased, Failure, Identity, Outcome, Refusal,
    fail_for,
};
use crate::channel::{Inbox, Mailbox};

/// A process-unique service number (the share and the counters key on
/// it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceId(pub u32);

static NEXT_SERVICE: AtomicU32 = AtomicU32::new(1);

/// Service-side loss counters ("count every loss": the ways a call or
/// its answer is lost on the service's side).
#[derive(Debug, Default)]
pub struct ServiceStats {
    /// Calls the service took out of its mailbox.
    pub calls_received: AtomicU64,
    /// Answers the caller's full completion channel refused (lost).
    pub replies_undeliverable_full: AtomicU64,
    /// Answers whose caller was gone (its table dropped, or an awaiting
    /// `…Handle` caller stopped waiting).
    pub replies_undeliverable_gone: AtomicU64,
    /// Calls the host's timeout ended (`TimedOut`: the game's
    /// `.timeout(d)` layer, or the host's default deadline).
    pub calls_timed_out: AtomicU64,
    /// Calls the host's load-shedding layer refused (`Refused(Shed)`).
    pub calls_shed: AtomicU64,
    /// Calls another layer of the host's stack failed (`Abandoned`).
    pub calls_failed: AtomicU64,
    /// Calls whose service task ended without answering (a panic in the
    /// service: `Abandoned`).
    pub calls_abandoned: AtomicU64,
    /// Per method (the generated `METHODS` order; empty for a
    /// hand-written service): calls finished and their latency.
    pub methods: Box<[MethodStats]>,
}

/// One method's finished calls and their service-side latency (from the
/// host taking the call out of its mailbox to its answer).
#[derive(Debug, Default)]
pub struct MethodStats {
    /// The method's name.
    pub name: &'static str,
    /// Calls finished (every ending).
    pub calls: AtomicU64,
    /// Their summed latency, microseconds.
    pub latency_us_total: AtomicU64,
    /// The largest latency seen, microseconds.
    pub latency_us_max: AtomicU64,
}

impl MethodStats {
    pub(crate) fn record(&self, us: u64) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.latency_us_total.fetch_add(us, Ordering::Relaxed);
        self.latency_us_max.fetch_max(us, Ordering::Relaxed);
    }
}

impl ServiceStats {
    /// A snapshot of the three counters, in declaration order.
    pub fn values(&self) -> [u64; 3] {
        [
            self.calls_received.load(Ordering::Relaxed),
            self.replies_undeliverable_full.load(Ordering::Relaxed),
            self.replies_undeliverable_gone.load(Ordering::Relaxed),
        ]
    }
}

/// What a service's mailbox carries.
pub enum Mail<M> {
    /// One call.
    Call(Envelope<M>),
    /// The host's stop request ([`crate::service::Service`]'s stop): the
    /// service serves what was queued ahead of it, then ends.
    Stop,
}

/// One call in a service's mailbox: its context and the service's own
/// message (whose variants carry [`Ask`]s).
pub struct Envelope<M> {
    /// The key, the calling actor and the verified player.
    pub cx: CallCx,
    /// The service's message.
    pub msg: M,
}

/// A clonable handle on one service's mailbox.
pub struct Route<M> {
    pub(crate) id: ServiceId,
    pub(crate) name: &'static str,
    pub(crate) tx: Mailbox<Mail<M>>,
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

impl<M> Route<M> {
    /// A new service mailbox of `capacity` calls: the route the callers
    /// hold and the inbox the service drains.
    pub fn new(name: &'static str, capacity: usize) -> (Self, Inbox<Mail<M>>) {
        Self::with_methods(name, capacity, &[])
    }

    /// [`Self::new`] with per-method stats (`methods`, in the order the
    /// host indexes them).
    pub fn with_methods(
        name: &'static str,
        capacity: usize,
        methods: &[&'static str],
    ) -> (Self, Inbox<Mail<M>>) {
        let (tx, rx) = crate::channel::channel(capacity);
        let stats = ServiceStats {
            methods: methods
                .iter()
                .map(|name| MethodStats {
                    name,
                    ..MethodStats::default()
                })
                .collect(),
            ..ServiceStats::default()
        };
        let route = Self {
            id: ServiceId(NEXT_SERVICE.fetch_add(1, Ordering::Relaxed)),
            name,
            tx,
            stats: Arc::new(stats),
        };
        (route, rx)
    }

    /// The service's number.
    pub fn id(&self) -> ServiceId {
        self.id
    }

    /// The service's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The service's loss counters.
    pub fn stats(&self) -> &Arc<ServiceStats> {
        &self.stats
    }

    /// The stop request (non-blocking, the stop-message idiom).
    pub fn stop(&self)
    where
        M: Send + 'static,
    {
        crate::channel::post(&self.tx, Mail::Stop);
    }
}

/// One finished call on its way back to its table.
pub(crate) struct Completion {
    pub(crate) key: CallKey,
    pub(crate) outcome: Erased,
}

pub(crate) enum SinkTo {
    Actor(Mailbox<Completion>),
    /// An awaiting `…Handle` caller.
    Await(oneshot::Sender<Erased>),
    Spent,
}

/// The typed-at-birth, erased-in-flight way back. Dropping it unanswered
/// answers `Abandoned` at once (unless it was made quiet: the built-in
/// `External` worker, whose deadline the table's sweep owns).
pub(crate) struct ReplySink {
    pub(crate) key: CallKey,
    pub(crate) to: SinkTo,
    pub(crate) fail: fn(Failure) -> Erased,
    pub(crate) stats: Arc<ServiceStats>,
    pub(crate) quiet: bool,
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

    /// The call never left (refused at the mailbox): no answer is owed.
    pub(crate) fn disarm(mut self) {
        self.to = SinkTo::Spent;
    }
}

impl Drop for ReplySink {
    fn drop(&mut self) {
        if self.quiet {
            return;
        }
        let to = std::mem::replace(&mut self.to, SinkTo::Spent);
        if !matches!(to, SinkTo::Spent) {
            let sink = ReplySink {
                key: self.key,
                to,
                fail: self.fail,
                stats: Arc::clone(&self.stats),
                quiet: true,
            };
            sink.complete((self.fail)(Failure::Abandoned));
        }
    }
}

/// One call of type `C` inside a service's message: its arguments and
/// the right to answer it, once. Dropping it unanswered answers
/// [`CallError::Abandoned`].
pub struct Ask<C: Call> {
    /// The call's arguments.
    pub call: C,
    pub(crate) sink: ReplySink,
    pub(crate) _c: PhantomData<fn() -> C>,
}

impl<C: Call> Ask<C> {
    /// Answer the call (the service's typed result).
    pub fn answer(self, result: Result<C::Response, C::Error>) {
        let out: Outcome<C> = result.map_err(CallError::Service);
        self.sink.complete(Box::new(out));
    }

    /// End the call with an engine-side error (a host's timeout or shed
    /// layer).
    pub fn fail(self, error: CallError<C::Error>) {
        let out: Outcome<C> = Err(error);
        self.sink.complete(Box::new(out));
    }

    /// The call's arguments and its answer right, apart: what a host
    /// that runs the call through layers keeps (the generated
    /// `ServiceKind::split`). The [`Reply`] answers the call once;
    /// dropping it unanswered answers `Abandoned`.
    pub fn split(self) -> (C, Reply) {
        (self.call, Reply { sink: self.sink })
    }
}

/// A type-erased outcome (`Box<Outcome<C>>`): what a generated router
/// returns and a [`Reply`] carries back.
pub type AnyOutcome = Box<dyn std::any::Any + Send + Sync>;

/// A call's answer right, apart from its arguments ([`Ask::split`]).
/// Answers once; dropping it unanswered answers `Abandoned`.
pub struct Reply {
    pub(crate) sink: ReplySink,
}

impl Reply {
    /// Answer with the outcome the call's router produced (an
    /// `Outcome<C>` of the call's own type; anything else ends the call
    /// `Abandoned` at its caller).
    pub fn complete(self, outcome: AnyOutcome) {
        self.sink.complete(outcome);
    }

    /// End the call `TimedOut`.
    pub fn timed_out(self) {
        let out = (self.sink.fail)(Failure::TimedOut);
        self.sink.complete(out);
    }

    /// End the call `Refused(why)`.
    pub fn refuse(self, why: Refusal) {
        let out = (self.sink.fail)(Failure::Refused(why));
        self.sink.complete(out);
    }

    /// End the call `Abandoned`.
    pub fn abandon(self) {
        let out = (self.sink.fail)(Failure::Abandoned);
        self.sink.complete(out);
    }
}

impl ActorId {
    /// The calling "actor" of an awaitable `…Handle` call (a service,
    /// admin or boot code — outside every room's record).
    pub const HANDLE: ActorId = ActorId(u64::MAX);
}

/// Whom an awaitable `…Handle` call acts for: the verified player and
/// the trace it joins (the generated `within(cx)` / `on_behalf_of(&id)`).
#[derive(Debug, Clone, Default)]
pub struct OnBehalf {
    player: Option<Identity>,
    trace: Option<u64>,
}

impl OnBehalf {
    /// The same player and trace as the call `cx` (a call made from
    /// inside a service call).
    pub fn within(cx: &CallCx) -> Self {
        Self {
            player: cx.player().cloned(),
            trace: Some(cx.trace_id()),
        }
    }

    /// This verified player (admin, boot code); a fresh trace.
    pub fn player(who: &Identity) -> Self {
        Self {
            player: Some(who.clone()),
            trace: None,
        }
    }
}

static HANDLE_SEQ: AtomicU64 = AtomicU64::new(1);
/// Awaitable calls are in no room's record: their keys only need to be
/// unique in the process, not reproducible.
const HANDLE_ORIGIN: u64 = 0x4841_4e44_4c45_0000;

impl<M: Send + 'static> Route<M> {
    /// Call and await the answer (the generated `…Handle` methods). A
    /// full or gone mailbox ends it at once (`Refused(Full)` /
    /// `Refused(Gone)`: on the awaitable path "at once" is meaningful);
    /// otherwise the one await is the answer's oneshot, which always
    /// completes (a dropped answer right answers `Abandoned`).
    pub async fn ask<C: Call>(&self, call: C, wrap: fn(Ask<C>) -> M, on: OnBehalf) -> Outcome<C> {
        let key = CallKey {
            origin: HANDLE_ORIGIN,
            seq: HANDLE_SEQ.fetch_add(1, Ordering::Relaxed),
        };
        let (tx, rx) = oneshot::channel();
        let cx = CallCx {
            key,
            caller: Caller {
                actor: ActorId::HANDLE,
                player: on.player,
            },
            parent: on.trace,
        };
        match self.tx.try_reserve() {
            Ok(permit) => {
                let ask = Ask {
                    call,
                    sink: ReplySink {
                        key,
                        to: SinkTo::Await(tx),
                        fail: fail_for::<C>,
                        stats: Arc::clone(&self.stats),
                        quiet: false,
                    },
                    _c: PhantomData,
                };
                permit.send(Mail::Call(Envelope { cx, msg: wrap(ask) }));
            }
            Err(mpsc::error::TrySendError::Full(())) => {
                return Err(CallError::Refused(Refusal::Full));
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                return Err(CallError::Refused(Refusal::Gone));
            }
        }
        match rx.await {
            Ok(erased) => match erased.downcast::<Outcome<C>>() {
                Ok(o) => *o,
                Err(_) => Err(CallError::Abandoned),
            },
            Err(_) => Err(CallError::Abandoned),
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

/// A built call, not yet issued: hand it to
/// [`Calls::call`](crate::calls::Calls::call).
pub struct CallSpec<C: Call> {
    pub(crate) service: ServiceId,
    pub(crate) name: &'static str,
    pub(crate) stats: Arc<ServiceStats>,
    pub(crate) send: SendFn,
    pub(crate) _c: PhantomData<fn() -> C>,
}

impl<C: Call> CallSpec<C> {
    /// `call` for the service behind `route`, wrapped into its message by
    /// `wrap` (a generated client does this; a hand-written one passes
    /// its enum variant: `CallSpec::new(&bank, Transfer { .. }, BankMsg::Transfer)`).
    pub fn new<M: Send + 'static>(route: &Route<M>, call: C, wrap: fn(Ask<C>) -> M) -> Self {
        let tx = route.tx.clone();
        let send: SendFn = Arc::new(move |cx, sink| {
            // Reserve first: a refused send keeps the sink (no answer is
            // owed for a call that never left).
            match tx.try_reserve() {
                Ok(permit) => {
                    let ask = Ask {
                        call: call.clone(),
                        sink,
                        _c: PhantomData,
                    };
                    permit.send(Mail::Call(Envelope { cx, msg: wrap(ask) }));
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Full(())) => Err((SendFail::Full, sink)),
                Err(mpsc::error::TrySendError::Closed(())) => Err((SendFail::Gone, sink)),
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
