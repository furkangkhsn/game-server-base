//! The typed contract (SERVICES.md §4): [`Call`] for in-process calls,
//! [`Request`] for the client wire, identities, keys, outcomes.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::id::PlayerId;

/// The wire form of a client-facing message. Every `prost` message has
/// it (a blanket impl); a game that wants another format implements it
/// on its own types.
pub trait WireMessage: Sized + Send + Sync + 'static {
    /// Encode for the wire.
    fn to_wire(&self) -> Vec<u8>;
    /// Decode from the wire.
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
/// no wire. `#[gsb::service]` (q2) generates one per method; `.proto`
/// codegen (Faz 3) one per `rpc`, together with [`Request`].
///
/// `Clone` (and `Clone` answers): the table keeps what it needs to
/// re-issue a call after a migration and to copy an outcome into the
/// input log (SERVICES.md §4.2, §19 S5).
pub trait Call: Clone + Send + Sync + 'static {
    /// The successful answer.
    type Response: Clone + Send + Sync + 'static;
    /// The service's own typed error.
    type Error: Clone + Send + Sync + 'static;
}

/// A client-wire request: a [`Call`] whose request, answer and error all
/// have a wire form, plus its opcode (the inner op of the `RpcRequest`
/// envelope).
pub trait Request: Call<Response: WireMessage, Error: WireMessage> + WireMessage {
    /// The request's inner opcode. Never reused; a collision is a
    /// [`BootError`](crate::calls::BootError).
    const OP: u16;
}

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

/// What a call (or its result) belongs to, opaque to the engine: an
/// entity's bits, a player id, anything the game can test for life
/// ([`GameLogic::owner_alive`](crate::room::GameLogic::owner_alive)).
/// The engine's default owner of a session is its player
/// (`Owner::from(PlayerId)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Owner(pub u64);

impl Owner {
    /// "The actor itself": a call without `.owner(..)`; always alive.
    pub const ACTOR: Owner = Owner(u64::MAX);
}

impl From<PlayerId> for Owner {
    fn from(p: PlayerId) -> Self {
        Owner(p.0)
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
