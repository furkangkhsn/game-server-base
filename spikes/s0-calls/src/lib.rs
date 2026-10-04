//! Round s0b spike: service calls through an ECS-agnostic `Calls` table.
//!
//! The design is `docs/SERVICES.md`; this crate proves it on its own
//! minimal tick without touching the engine. It is NOT a member of the
//! gsb workspace.
//!
//! - [`contract`]: `Call` (in-process), `Request` (client wire), keys,
//!   identities, outcomes;
//! - `#[service]` (the `s0-calls-macros` crate): a plain async trait →
//!   call types, a tower router, a non-blocking `…Client`, an awaitable
//!   `…Handle`;
//! - [`calls`]: the table — `take_incoming`, `call(..).owner().with().answers()`,
//!   `take_resolved`, `in_flight`, `forward`, `export`/`import`;
//! - [`room`]: `CONTROL → RESPONSES → READ → CONVERT → SYSTEMS → BROADCAST`,
//!   recording and replay;
//! - [`live`] (tokio + tower), [`sim`] (test mode), `ReplayHost`;
//! - [`ecs`]: the bevy, hecs and plain flavours.
//!
//! # Types are checked end to end
//!
//! The shape that compiles: a service method, a client request with the
//! same answer and error, a room that answers the request with the call.
//!
//! ```
//! use s0_calls::*;
//!
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Buy { #[prost(uint32, tag = "1")] pub price: u32 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! // What `.proto` codegen emits for `rpc Buy(Buy) returns (Bought) { option (gsb.op) = 1; }`:
//! impl Call for Buy { type Response = Bought; type Error = Nope; }
//! impl Request for Buy { const OP: u16 = 1; }
//!
//! #[service]
//! pub trait Shop {
//!     async fn buy(&self, cx: &CallCx, price: u32) -> Result<Bought, Nope>;
//! }
//!
//! fn system(calls: &mut Calls, shop: &ShopClient) {
//!     for r in calls.take_incoming::<Buy>() {
//!         calls.call(shop.buy(r.body.price)).answers(r.reply);
//!     }
//! }
//! # fn main() {}
//! ```
//!
//! (Stable rustdoc does not check error codes on `compile_fail`; each
//! block below was also compiled by hand and fails with the code named.)
//!
//! **A call whose answer is not the request's answer cannot `.answers` it**
//! (E0271, type mismatch resolving `<Buy as Call>::Response == u64`):
//!
//! ```compile_fail
//! use s0_calls::*;
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Buy { #[prost(uint32, tag = "1")] pub price: u32 }
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! # impl Call for Buy { type Response = Bought; type Error = Nope; }
//! # impl Request for Buy { const OP: u16 = 1; }
//! #[service]
//! pub trait Shop {
//!     async fn price(&self, cx: &CallCx, item: u32) -> Result<u64, Nope>;
//! }
//! fn system(calls: &mut Calls, shop: &ShopClient) {
//!     for r in calls.take_incoming::<Buy>() {
//!         calls.call(shop.price(r.body.price)).answers(r.reply);
//!     }
//! }
//! # fn main() {}
//! ```
//!
//! **A request is answered with its own answer type** (E0308):
//!
//! ```compile_fail
//! use s0_calls::*;
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Buy { #[prost(uint32, tag = "1")] pub price: u32 }
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! # impl Call for Buy { type Response = Bought; type Error = Nope; }
//! # impl Request for Buy { const OP: u16 = 1; }
//! fn system(calls: &mut Calls) {
//!     for r in calls.take_incoming::<Buy>() {
//!         calls.reply(r.reply, Ok(Nope { code: 1 }));
//!     }
//! }
//! # fn main() {}
//! ```
//!
//! **A service implementation returns the declared answer** (E0271/E0308
//! on the impl's `async fn`):
//!
//! ```compile_fail
//! use s0_calls::*;
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! #[service]
//! pub trait Shop {
//!     async fn buy(&self, cx: &CallCx, price: u32) -> Result<Bought, Nope>;
//! }
//! struct Fake;
//! impl Shop for Fake {
//!     async fn buy(&self, _cx: &CallCx, price: u32) -> Result<u32, Nope> { Ok(price) }
//! }
//! # fn main() {}
//! ```
//!
//! **A resolved outcome has its call's answer type** (E0308):
//!
//! ```compile_fail
//! use s0_calls::*;
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! #[service]
//! pub trait Shop {
//!     async fn buy(&self, cx: &CallCx, price: u32) -> Result<Bought, Nope>;
//! }
//! fn system(calls: &mut Calls) {
//!     for r in calls.take_resolved::<shop::Buy, ()>() {
//!         let n: u32 = r.outcome.unwrap();
//!     }
//! }
//! # fn main() {}
//! ```
//!
//! **A forward rule's call answers what the request expects** (E0271):
//!
//! ```compile_fail
//! use s0_calls::*;
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Buy { #[prost(uint32, tag = "1")] pub price: u32 }
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! # #[derive(Clone, PartialEq, prost::Message)]
//! # pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! # impl Call for Buy { type Response = Bought; type Error = Nope; }
//! # impl Request for Buy { const OP: u16 = 1; }
//! #[service]
//! pub trait Shop {
//!     async fn price(&self, cx: &CallCx, item: u32) -> Result<u64, Nope>;
//! }
//! fn boot(calls: &mut Calls, shop: ShopClient) {
//!     calls.forward(move |r: Buy| shop.price(r.price)).unwrap();
//! }
//! # fn main() {}
//! ```

pub mod calls;
pub mod contract;
pub mod ecs;
pub mod live;
pub mod room;
pub mod route;
pub mod sim;
pub mod wire;

pub use calls::{
    AnyIncoming, BootError, CallBuilder, Calls, CallsConfig, Counters, Exported, Incoming,
    Recorded, ReplyTo, Resolved,
};
pub use contract::{
    ActorId, Call, CallCx, CallError, CallKey, Caller, ConnId, DecodeError, Erased, Identity,
    Outcome, Owner, Refusal, Request, WireMessage,
};
pub use live::{ReplayHost, Server, Setup};
pub use room::{Control, InputLog, Inputs, Rng, Room, RoomConfig, StateHasher, TickCtx};
pub use route::{
    CallSpec, Delay, Dispatch, Host, Route, Served, ServiceClient, ServiceDef, ServiceKind,
    ServiceStats,
};
pub use s0_calls_macros::service;
pub use sim::Sim;

#[doc(hidden)]
pub mod __private {
    pub use crate::route::{BoxError, BoxFuture};
    pub use tower;
}
