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

use tokio::sync::mpsc;

use crate::calls::contract::{Call, CallCx, CallError, CallKey, Erased, Failure, Outcome};
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
    /// Answers whose caller was gone (its table dropped).
    pub replies_undeliverable_gone: AtomicU64,
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
        let (tx, rx) = crate::channel::channel(capacity);
        let route = Self {
            id: ServiceId(NEXT_SERVICE.fetch_add(1, Ordering::Relaxed)),
            name,
            tx,
            stats: Arc::new(ServiceStats::default()),
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
        if let SinkTo::Actor(tx) = to {
            match tx.try_send(Completion {
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
            }
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
        if let SinkTo::Actor(tx) = std::mem::replace(&mut self.to, SinkTo::Spent) {
            let sink = ReplySink {
                key: self.key,
                to: SinkTo::Actor(tx),
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
