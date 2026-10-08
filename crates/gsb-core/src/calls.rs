//! Service calls and client requests through one per-actor table
//! (`docs/SERVICES.md`, Faz 1 — round q1).
//!
//! Every tick actor (a room, a shard) owns one [`Calls`] table and lends
//! it to the game ([`GameLogic::update_with_calls`](crate::room::GameLogic::update_with_calls),
//! [`GameLogic::boot_calls`](crate::room::GameLogic::boot_calls)). The
//! table is ECS-agnostic — a plain struct, no ECS dependency; the ECS
//! glue lives in optional adapter crates (SERVICES.md §5.5, Faz 5).
//!
//! ```text
//! CONTROL → RESPONSES → READ → CONVERT → SYSTEMS → (FLUSH) → BROADCAST
//! ```
//!
//! - requests come IN through it: a request whose op the table claims
//!   ([`Calls::accept`], [`Calls::forward`]) is decoded at READ, typed
//!   ([`Incoming<R>`]), bound to its session's [`Owner`]; the game takes
//!   it ([`Calls::take_incoming`], [`Calls::take_incoming_all`]) and
//!   answers it ([`Calls::reply`], or a call's `.answers(..)`);
//! - calls go OUT through it ([`Calls::call`] → [`CallBuilder`]), never
//!   blocking, never failing on the spot: a refusal is decided at FLUSH
//!   and delivered at the next tick's RESPONSES like every other ending;
//! - results come BACK through it ([`Calls::take_resolved`]).
//!
//! What nobody takes or answers is closed at tick end and counted
//! ([`CallCounts`](crate::metrics::CallCounts)). An op the table does
//! not claim still goes to `GameLogic::handle_request`; its
//! `RequestDecision::External` runs as a built-in call of this table
//! (`engine.rs`). The service side is a [`Route`] (a bounded mailbox);
//! [`serve`] is the minimal host for a hand-written service, and the
//! live tower host ([`Services`], [`spawn`]) runs a `#[service]` trait's
//! implementation (`live.rs`, Faz 2 — round q2).

mod builder;
mod contract;
mod engine;
mod host;
mod live;
mod macro_checks;
#[cfg(feature = "record")]
mod record;
mod route;
mod table;

#[cfg(test)]
mod tests;

pub use builder::CallBuilder;
#[cfg(feature = "record")]
pub(crate) use contract::Erased;
pub use contract::{
    ActorId, Call, CallCx, CallError, CallKey, Caller, DecodeError, Identity, Outcome, Owner,
    Refusal, Request, WireMessage, mix,
};
pub use host::serve;
pub use live::{
    BoxError, BoxFuture, DEFAULT_MAILBOX, DEFAULT_SERVICE_TIMEOUT, Dispatch, EngineFuture,
    EngineHandler, EngineSubscription, RegisterError, Served, ServiceClient, ServiceDef,
    ServiceKind, Services, Setup, spawn,
};
pub use route::{
    AnyOutcome, Ask, CallSpec, Envelope, Mail, MethodStats, OnBehalf, Reply, Route, ServiceId,
    ServiceStats,
};
pub use table::{
    AnyIncoming, BootError, Calls, CallsConfig, DEFAULT_CALL_TIMEOUT, Exported, Incoming, ReplyTo,
    Resolved,
};
