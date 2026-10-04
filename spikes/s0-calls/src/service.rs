//! The service side of a call (SERVICES.md §4.2): what a game's service
//! actor receives and how it answers. The service is ordinary game code —
//! `gsb_core::service` style: a bounded mailbox, one task, a single await
//! per receive; its body is HTTP / NATS / a database / anything.

use std::any::Any;
use std::marker::PhantomData;

use tokio::sync::mpsc;

use crate::calls::CallKey;
use crate::request::Request;

/// A room's handle to a game service whose mailbox carries `M` (the
/// game's own enum, one variant per request kind it serves:
/// `M: From<Call<R>>` is "this service serves `R`" — calling a service
/// with a request it does not serve is a compile error).
pub struct ServiceRef<M> {
    name: &'static str,
    tx: mpsc::Sender<M>,
}

impl<M> Clone for ServiceRef<M> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            tx: self.tx.clone(),
        }
    }
}

impl<M> ServiceRef<M> {
    /// A service named `name` (its counters and caps are keyed by it)
    /// with a bounded mailbox of `capacity`.
    pub fn channel(name: &'static str, capacity: usize) -> (Self, mpsc::Receiver<M>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { name, tx }, rx)
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn sender(&self) -> &mpsc::Sender<M> {
        &self.tx
    }
}

/// One call as the service receives it.
pub struct Call<R: Request> {
    /// Unique per call (and stable across a re-issue): the service's
    /// idempotency key.
    pub key: CallKey,
    pub req: R,
    pub reply: Reply<R>,
}

/// The way back. Typed: a service can only answer `R` with
/// `Result<R::Response, R::Error>`. Dropping it unanswered reports
/// "abandoned" to the room at once (no waiting for the timeout).
pub struct Reply<R: Request> {
    key: CallKey,
    attempt: u64,
    to: Option<mpsc::Sender<Completion>>,
    _r: PhantomData<fn(R)>,
}

/// The room could not take the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undeliverable {
    /// The room's completion channel is full (the room will time the
    /// call out and count it).
    Full,
    /// The room (that incarnation) is gone.
    Gone,
}

impl<R: Request> Reply<R> {
    pub(crate) fn new(key: CallKey, attempt: u64, to: mpsc::Sender<Completion>) -> Self {
        Self {
            key,
            attempt,
            to: Some(to),
            _r: PhantomData,
        }
    }

    pub fn key(&self) -> CallKey {
        self.key
    }

    /// Answer the call. Never waits.
    pub fn send(mut self, result: Result<R::Response, R::Error>) -> Result<(), Undeliverable> {
        let Some(to) = self.to.take() else {
            return Err(Undeliverable::Gone);
        };
        to.try_send(Completion {
            key: self.key,
            attempt: self.attempt,
            body: Body::Answer(Box::new(result)),
        })
        .map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => Undeliverable::Full,
            mpsc::error::TrySendError::Closed(_) => Undeliverable::Gone,
        })
    }
}

impl<R: Request> Drop for Reply<R> {
    fn drop(&mut self) {
        if let Some(to) = self.to.take() {
            // Best effort: a full channel leaves the call to the timeout.
            let _ = to.try_send(Completion {
                key: self.key,
                attempt: self.attempt,
                body: Body::Abandoned,
            });
        }
    }
}

/// What travels back to the room: the key, the attempt (so a late answer
/// of an earlier attempt under the same key can never be taken for the
/// current one) and the type-erased answer. The erasure is internal: the
/// pending entry that reads it was created with the same `R`.
pub(crate) struct Completion {
    pub(crate) key: CallKey,
    pub(crate) attempt: u64,
    pub(crate) body: Body,
}

pub(crate) enum Body {
    Answer(Box<dyn Any + Send>),
    Abandoned,
}
