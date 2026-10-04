//! The typed contract (SERVICES.md §4): `Call` for in-process calls,
//! `Request` for the client wire, identities, keys, outcomes.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

/// Wire format of a client-facing message. prost by default (a blanket
/// impl); a game that wants another format implements it itself.
pub trait WireMessage: Sized + Send + Sync + 'static {
    fn to_wire(&self) -> Vec<u8>;
    fn from_wire(bytes: &[u8]) -> Result<Self, DecodeError>;
}

/// A payload did not decode as the registered type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeError;

impl<T: prost::Message + Default + Send + Sync + 'static> WireMessage for T {
    fn to_wire(&self) -> Vec<u8> {
        self.encode_to_vec()
    }
    fn from_wire(bytes: &[u8]) -> Result<Self, DecodeError> {
        T::decode(bytes).map_err(|_| DecodeError)
    }
}

/// One in-process call: its answer type and its error type. No opcode,
/// no wire. `#[service]` generates one per method; `.proto` codegen
/// generates one per `rpc` (together with [`Request`]).
///
/// `Clone` (and `Clone` answers) because the table keeps what it needs to
/// re-issue a call after a migration and to copy an outcome into the
/// input log (SERVICES.md §9; an open question asks whether to require a
/// serialisable form instead).
pub trait Call: Clone + Send + Sync + 'static {
    type Response: Clone + Send + Sync + 'static;
    type Error: Clone + Send + Sync + 'static;
}

/// A client-wire request: a [`Call`] whose request, answer and error all
/// have a wire form, plus its opcode. Generated from a `.proto` `service`
/// block with `option (gsb.op) = N`.
pub trait Request: Call<Response: WireMessage, Error: WireMessage> + WireMessage {
    const OP: u16;
}

/// A verified identity (the ticket's player). Never taken from a payload.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity(Arc<str>);

impl Identity {
    pub fn new(s: &str) -> Self {
        Self(Arc::from(s))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", &*self.0)
    }
}

/// A transport session (the engine's `ConnectionId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// What a call (or its result) belongs to, opaque to the engine: an
/// entity's bits, a player index, anything the game can test for life.
/// The ECS adapters add `.entity()` / `.to_owner()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Owner(pub u64);

impl Owner {
    /// "The actor itself": a call without `.owner(..)`; always alive.
    pub const ACTOR: Owner = Owner(u64::MAX);
}

/// Which actor made a call (a room, a scheduler task, an admin task).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActorId(pub u64);

impl ActorId {
    /// Calls made through an awaitable `…Handle` (services, admin).
    pub const HANDLE: ActorId = ActorId(u64::MAX);
}

/// A call's key: unique, stable across a re-issue, the service's
/// idempotency key. Rooms derive `origin` from their recorded seed, so a
/// replay produces the same keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallKey {
    pub origin: u64,
    pub seq: u64,
}

impl CallKey {
    /// The trace id the call's span carries (derived, so it survives a
    /// re-issue: the re-issued call joins the same trace).
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

/// Who called: the actor, and the verified player the call acts for
/// (set by `.answers(req)` from the request's identity, or by
/// `.on_behalf_of(identity)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub actor: ActorId,
    pub player: Option<Identity>,
}

/// What a service method receives besides its arguments.
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
    pub fn caller(&self) -> &Caller {
        &self.caller
    }
    /// The verified player this call acts for, if any.
    pub fn player(&self) -> Option<&Identity> {
        self.caller.player.as_ref()
    }
    pub fn trace_id(&self) -> u64 {
        self.key.trace_id()
    }
}

/// Why the engine refused a call before the service saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The service's mailbox was full.
    Full,
    /// The service is gone (stopped, or never registered in this host).
    Gone,
    /// This caller's share of the service (in-flight cap) was used up.
    Share,
    /// The service's `load_shed` layer shed it.
    Shed,
}

/// A call's failure: the service's own typed error, or the engine's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError<E> {
    Service(E),
    Refused(Refusal),
    /// The service's `timeout` layer fired.
    TimedOut,
    /// The service dropped the call unanswered (panic, stop, bug).
    Abandoned,
}

/// What a call ends with.
pub type Outcome<C> = Result<<C as Call>::Response, CallError<<C as Call>::Error>>;

/// A type-erased outcome (`Box<Outcome<C>>`) on its way back.
pub type Erased = Box<dyn Any + Send + Sync>;

/// An engine-side failure, before it is typed for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Refused(Refusal),
    TimedOut,
    Abandoned,
}

pub(crate) fn fail_for<C: Call>(f: Failure) -> Erased {
    let e: CallError<C::Error> = match f {
        Failure::Refused(r) => CallError::Refused(r),
        Failure::TimedOut => CallError::TimedOut,
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
    Refused(Refusal),
    TimedOut,
    Abandoned,
}

pub(crate) fn ended_of<C: Call>(o: &Erased) -> Ended {
    match o.downcast_ref::<Outcome<C>>() {
        Some(Ok(_)) => Ended::Ok,
        Some(Err(CallError::Service(_))) => Ended::ServiceError,
        Some(Err(CallError::Refused(r))) => Ended::Refused(*r),
        Some(Err(CallError::TimedOut)) => Ended::TimedOut,
        Some(Err(CallError::Abandoned)) | None => Ended::Abandoned,
    }
}

pub(crate) fn clone_outcome<C: Call>(o: &Erased) -> Erased {
    let o = o
        .downcast_ref::<Outcome<C>>()
        .expect("an outcome is always stored as its call's type");
    Box::new(o.clone())
}
