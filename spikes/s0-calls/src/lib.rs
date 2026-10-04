//! Round s0 spike: calls, responses and events in a synchronous tick.
//!
//! The design is `docs/SERVICES.md`; this crate proves its riskiest
//! parts on its own minimal tick ([`room::Room`]) without touching the
//! engine. It is NOT a workspace member.
//!
//! - [`request`]: the typed contract (`Request`, `Event`), the request
//!   table that refuses duplicate opcodes and answers malformed payloads
//!   itself;
//! - [`service`]: what a game service receives (`Call<R>`) and its typed
//!   way back (`Reply<R>`);
//! - [`calls`]: the call table — fail-fast refusal, the fairness cap,
//!   keys that double as idempotency keys, the RESPONSES reconcile;
//! - [`push`]: identity-addressed events to the client or to the room;
//! - [`room`]: `CONTROL → RESPONSES → READ → EVENTS → CONVERT → SYSTEMS →
//!   BROADCAST`;
//! - [`fake`]: a fake service on the test clock;
//! - [`client`]: the client core's poll shape (responses and pushes as
//!   separate event kinds), typed by the same traits.
//!
//! # A wrong response type does not compile
//!
//! (Stable rustdoc does not enforce the error codes on `compile_fail`;
//! each block below was also compiled by hand in round s0 and fails with
//! exactly the code it names — see SERVICES.md §12.)
//!
//! The shape that compiles: a service answers `Buy` with
//! `Result<Bought, Nope>`.
//!
//! ```
//! use s0_calls::request::Request;
//! use s0_calls::service::Reply;
//!
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Buy { #[prost(uint32, tag = "1")] pub price: u32 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! impl Request for Buy { const OP: u16 = 1; type Response = Bought; type Error = Nope; }
//!
//! fn answer(reply: Reply<Buy>) {
//!     let _ = reply.send(Ok(Bought { balance: 100 }));
//! }
//! ```
//!
//! The same service answering with another message type is rejected by
//! the compiler (mismatched types):
//!
//! ```compile_fail,E0308
//! use s0_calls::request::Request;
//! use s0_calls::service::Reply;
//!
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Buy { #[prost(uint32, tag = "1")] pub price: u32 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Refund { #[prost(uint64, tag = "1")] pub amount: u64 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! impl Request for Buy { const OP: u16 = 1; type Response = Bought; type Error = Nope; }
//!
//! fn answer(reply: Reply<Buy>) {
//!     let _ = reply.send(Ok(Refund { amount: 1 }));
//! }
//! ```
//!
//! So is the room answering a client request with the wrong type:
//!
//! ```compile_fail,E0308
//! use s0_calls::request::{ClientCall, Request};
//! use s0_calls::room::TickCtx;
//!
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Buy { #[prost(uint32, tag = "1")] pub price: u32 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Refund { #[prost(uint64, tag = "1")] pub amount: u64 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! impl Request for Buy { const OP: u16 = 1; type Response = Bought; type Error = Nope; }
//!
//! fn answer(ctx: &TickCtx<'_, ()>, call: ClientCall<Buy>) {
//!     ctx.reply(call, Ok(Refund { amount: 1 }));
//! }
//! ```
//!
//! …and a call whose tag builds something other than the logic's `Done`
//! type:
//!
//! ```compile_fail
//! use s0_calls::request::Request;
//! use s0_calls::room::TickCtx;
//! use s0_calls::service::{Call, ServiceRef};
//!
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Buy { #[prost(uint32, tag = "1")] pub price: u32 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Bought { #[prost(uint64, tag = "1")] pub balance: u64 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! impl Request for Buy { const OP: u16 = 1; type Response = Bought; type Error = Nope; }
//!
//! pub enum Econ { Buy(Call<Buy>) }
//! impl From<Call<Buy>> for Econ { fn from(c: Call<Buy>) -> Self { Econ::Buy(c) } }
//! pub enum MyDone { Bought(bool) }
//!
//! fn f(ctx: &TickCtx<'_, MyDone>, econ: &ServiceRef<Econ>) {
//!     let _ = ctx.call(econ, Buy { price: 1 }, |o| o.is_ok());
//! }
//! ```
//!
//! …and calling a service with a request it does not serve (with one
//! `From<Call<_>>` impl the compiler infers the served type and reports
//! "expected `Buy`, found `Chat`"):
//!
//! ```compile_fail,E0308
//! use s0_calls::request::Request;
//! use s0_calls::room::TickCtx;
//! use s0_calls::service::{Call, ServiceRef};
//!
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Buy { #[prost(uint32, tag = "1")] pub price: u32 }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Chat { #[prost(string, tag = "1")] pub text: String }
//! #[derive(Clone, PartialEq, prost::Message)]
//! pub struct Nope { #[prost(uint32, tag = "1")] pub code: u32 }
//! impl Request for Buy { const OP: u16 = 1; type Response = Nope; type Error = Nope; }
//! impl Request for Chat { const OP: u16 = 2; type Response = Nope; type Error = Nope; }
//!
//! pub enum Econ { Buy(Call<Buy>) }
//! impl From<Call<Buy>> for Econ { fn from(c: Call<Buy>) -> Self { Econ::Buy(c) } }
//!
//! fn f(ctx: &TickCtx<'_, ()>, econ: &ServiceRef<Econ>) {
//!     let _ = ctx.call(econ, Chat { text: "hi".into() }, |_| ());
//! }
//! ```

pub mod calls;
pub mod client;
pub mod fake;
pub mod push;
pub mod request;
pub mod room;
pub mod service;
pub mod wire;
