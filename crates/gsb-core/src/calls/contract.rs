//! The typed contract (SERVICES.md §4): [`Call`] for in-process calls,
//! [`Request`] for the client wire, identities, keys, outcomes.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

pub use crate::owner::Owner;

// The request contract (`Call`, `Request`, `WireMessage`) lives in the
// protocol crate, which the client shares; the core re-exports it.
pub use gsb_protocol::request::{Call, DecodeError, Request, WireMessage};

/// A verified identity (the ticket's player — the session's resume key).
/// Never taken from a payload.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity(Arc<str>);

impl Identity {
    /// Wrap an identity string.
    pub fn new(s: &str) -> Self {
        Self(Arc::from(s))
    }
    /// The identity as text (empty: an anonymous session).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", &*self.0)
    }
}

/// Which actor made a call (a room, a shard, a scheduler task …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActorId(pub u64);

/// A call's key: unique, stable across a re-issue (migration), the
/// service's idempotency key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallKey {
    /// The issuing table's origin (rooms: derived from the room id; Faz 4
    /// derives it from the recorded seed).
    pub origin: u64,
    /// The issuing table's sequence number.
    pub seq: u64,
}

impl CallKey {
    /// The trace id the call's span carries (derived, so a re-issued
    /// call joins the same trace).
    pub fn trace_id(&self) -> u64 {
        mix(self.origin ^ mix(self.seq))
    }
}

/// SplitMix64's finaliser: a cheap, well-spread, deterministic mix.
pub fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Who called: the actor, and the verified player the call acts for (set
/// by `.answers(req)` from the request's identity, or by
/// `.on_behalf_of(identity)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// The calling actor.
    pub actor: ActorId,
    /// The verified player, if any.
    pub player: Option<Identity>,
}

/// What a service receives besides its arguments.
#[derive(Debug, Clone)]
pub struct CallCx {
    pub(crate) key: CallKey,
    pub(crate) caller: Caller,
}

impl CallCx {
    /// The idempotency key: same key = same effect, once.
    pub fn key(&self) -> CallKey {
        self.key
    }
    /// The calling actor and player.
    pub fn caller(&self) -> &Caller {
        &self.caller
    }
    /// The verified player this call acts for, if any.
    pub fn player(&self) -> Option<&Identity> {
        self.caller.player.as_ref()
    }
    /// The call's trace id.
    pub fn trace_id(&self) -> u64 {
        self.key.trace_id()
    }
}

/// Why the engine refused a call before the service saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The service's mailbox was full.
    Full,
    /// The service is gone (stopped, or its mailbox closed).
    Gone,
    /// This caller's share of the service (its in-flight cap) was used up.
    Share,
    /// The service's load-shedding layer shed it (q2's tower host).
    Shed,
}

/// A call's failure: the service's own typed error, or the engine's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError<E> {
    /// The service answered with its typed error.
    Service(E),
    /// The engine refused the call (decided at FLUSH, delivered at the
    /// next tick's RESPONSES like every other ending).
    Refused(Refusal),
    /// The call's deadline passed.
    TimedOut,
    /// The service dropped the call unanswered (a panic, a stop, a bug).
    Abandoned,
}

/// What a call ends with.
pub type Outcome<C> = Result<<C as Call>::Response, CallError<<C as Call>::Error>>;

/// A type-erased outcome (`Box<Outcome<C>>`) on its way back.
pub(crate) type Erased = Box<dyn Any + Send + Sync>;

/// An engine-side failure, before it is typed for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Refused(Refusal),
    Abandoned,
}

pub(crate) fn fail_for<C: Call>(f: Failure) -> Erased {
    let e: CallError<C::Error> = match f {
        Failure::Refused(r) => CallError::Refused(r),
        Failure::Abandoned => CallError::Abandoned,
    };
    let out: Outcome<C> = Err(e);
    Box::new(out)
}

/// How an outcome ended (for the counters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    Ok,
    ServiceError,
    Refused,
    TimedOut,
    Abandoned,
}

pub(crate) fn ended_of<C: Call>(o: &Erased) -> Ended {
    match o.downcast_ref::<Outcome<C>>() {
        Some(Ok(_)) => Ended::Ok,
        Some(Err(CallError::Service(_))) => Ended::ServiceError,
        Some(Err(CallError::Refused(_))) => Ended::Refused,
        Some(Err(CallError::TimedOut)) => Ended::TimedOut,
        Some(Err(CallError::Abandoned)) | None => Ended::Abandoned,
    }
}
