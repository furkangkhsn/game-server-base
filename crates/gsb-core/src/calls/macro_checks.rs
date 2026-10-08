//! Compile-time checks of `#[gsb_core::service]` (doc-tests only; no
//! code). The first block is the shape that compiles; every other block
//! must NOT compile — each locks one rule the macro or the generated
//! types enforce.
//!
//! The shape that compiles: a trait, an impl, registration, both
//! clients.
//!
//! ```
//! use gsb_core::calls::{CallCx, Calls, Services};
//!
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn charge(&self, cx: &CallCx, amount: u64) -> Result<u64, String>;
//! }
//! struct Vault;
//! impl Bank for Vault {
//!     async fn charge(&self, _cx: &CallCx, amount: u64) -> Result<u64, String> {
//!         Ok(500 - amount)
//!     }
//! }
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() {
//!     let mut services = Services::new();
//!     services.service(Bank::serve(Vault), |s| s.timeout(std::time::Duration::from_secs(1))).expect("once");
//!     let bank: BankClient = services.client();
//!     let handle: BankHandle = services.client();
//!     let mut calls = Calls::default();
//!     calls.call(bank.charge(5)).with(());
//!     assert_eq!(handle.charge(100).await, Ok(400));
//! }
//! ```
//!
//! An impl whose method returns another type than the trait's.
//!
//! ```compile_fail
//! use gsb_core::calls::{CallCx, Calls, Services};
//!
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn charge(&self, cx: &CallCx, amount: u64) -> Result<u64, String>;
//! }
//! struct Vault;
//! impl Bank for Vault {
//!     async fn charge(&self, _cx: &CallCx, amount: u64) -> Result<u32, String> {
//!         Ok(500)
//!     }
//! }
//! fn main() {}
//! ```
//!
//! `serve` of a type that does not implement the service.
//!
//! ```compile_fail
//! use gsb_core::calls::{CallCx, Calls, Services};
//!
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn charge(&self, cx: &CallCx, amount: u64) -> Result<u64, String>;
//! }
//! struct Vault;
//! impl Bank for Vault {
//!     async fn charge(&self, _cx: &CallCx, amount: u64) -> Result<u64, String> {
//!         Ok(500 - amount)
//!     }
//! }
//! struct NotABank;
//! fn main() {
//!     let mut services = Services::new();
//!     services.service(Bank::serve(NotABank), |s| s).expect("once");
//! }
//! ```
//!
//! A client method the service does not have.
//!
//! ```compile_fail
//! use gsb_core::calls::{CallCx, Calls, Services};
//!
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn charge(&self, cx: &CallCx, amount: u64) -> Result<u64, String>;
//! }
//! struct Vault;
//! impl Bank for Vault {
//!     async fn charge(&self, _cx: &CallCx, amount: u64) -> Result<u64, String> {
//!         Ok(500 - amount)
//!     }
//! }
//! fn main() {
//!     let mut services = Services::new();
//!     services.service(Bank::serve(Vault), |s| s).expect("once");
//!     let bank: BankClient = services.client();
//!     let _ = bank.refund(5);
//! }
//! ```
//!
//! A client argument of the wrong type.
//!
//! ```compile_fail
//! use gsb_core::calls::{CallCx, Calls, Services};
//!
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn charge(&self, cx: &CallCx, amount: u64) -> Result<u64, String>;
//! }
//! struct Vault;
//! impl Bank for Vault {
//!     async fn charge(&self, _cx: &CallCx, amount: u64) -> Result<u64, String> {
//!         Ok(500 - amount)
//!     }
//! }
//! fn main() {
//!     let mut services = Services::new();
//!     services.service(Bank::serve(Vault), |s| s).expect("once");
//!     let bank: BankClient = services.client();
//!     let _ = bank.charge("five");
//! }
//! ```
//!
//! A method that is not `async`.
//!
//! ```compile_fail
//! use gsb_core::calls::CallCx;
//! #[gsb_core::service]
//! pub trait Bank {
//!     fn charge(&self, cx: &CallCx, amount: u64) -> Result<u64, String>;
//! }
//! fn main() {}
//! ```
//!
//! A method that does not return `Result<Response, Error>`.
//!
//! ```compile_fail
//! use gsb_core::calls::CallCx;
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn charge(&self, cx: &CallCx, amount: u64) -> u64;
//! }
//! fn main() {}
//! ```
//!
//! A method without the `cx: &CallCx` parameter.
//!
//! ```compile_fail
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn charge(&self, amount: u64) -> Result<u64, String>;
//! }
//! fn main() {}
//! ```
//!
//! A borrowed argument (a call travels to another task).
//!
//! ```compile_fail
//! use gsb_core::calls::CallCx;
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn charge(&self, cx: &CallCx, who: &str) -> Result<u64, String>;
//! }
//! fn main() {}
//! ```
//!
//! A method name the generated service uses itself.
//!
//! ```compile_fail
//! use gsb_core::calls::CallCx;
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn serve(&self, cx: &CallCx) -> Result<u64, String>;
//! }
//! fn main() {}
//! ```
//!
//! A service whose types are not `Clone` (a call is re-issued after a
//! migration).
//!
//! ```compile_fail
//! use gsb_core::calls::CallCx;
//! pub struct Coin;
//! #[gsb_core::service]
//! pub trait Bank {
//!     async fn charge(&self, cx: &CallCx, coin: Coin) -> Result<u64, String>;
//! }
//! fn main() {}
//! ```
//!
//! Generics on the trait.
//!
//! ```compile_fail
//! use gsb_core::calls::CallCx;
//! #[gsb_core::service]
//! pub trait Bank<T> {
//!     async fn charge(&self, cx: &CallCx, amount: u64) -> Result<u64, String>;
//! }
//! fn main() {}
//! ```
//!
//! ## Engine events (`#[gsb(engine_events)]`, round e4)
//!
//! The shape that compiles: one marked method beside the calls, taking
//! the event (or the notice, with its number); registered with
//! `Setup::engine_events`, it yields one subscription for the registry.
//! A trait may hold only the engine method.
//!
//! ```
//! use gsb_core::calls::{CallCx, Services};
//! use gsb_core::events::{EngineEvent, EngineNotice, Interest};
//!
//! #[gsb_core::service]
//! pub trait Friends {
//!     async fn count(&self, cx: &CallCx) -> Result<u32, String>;
//!     /// Presence.
//!     #[gsb(engine_events)]
//!     async fn on_engine(&self, e: EngineEvent);
//! }
//! #[gsb_core::service]
//! pub trait Audit {
//!     #[gsb(engine_events)]
//!     async fn seen(&self, n: EngineNotice);
//! }
//! struct Graph;
//! impl Friends for Graph {
//!     async fn count(&self, _cx: &CallCx) -> Result<u32, String> {
//!         Ok(0)
//!     }
//!     async fn on_engine(&self, _e: EngineEvent) {}
//! }
//! struct Log;
//! impl Audit for Log {
//!     async fn seen(&self, _n: EngineNotice) {}
//! }
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() {
//!     let mut services = Services::new();
//!     services
//!         .service(Friends::serve(Graph), |s| s.engine_events(Interest::PRESENCE))
//!         .expect("once");
//!     services.service(Audit::serve(Log), |s| s.engine_events(Interest::ALL)).expect("once");
//!     let subs = services.take_engine_subscriptions();
//!     assert_eq!(subs.iter().map(|s| s.name).collect::<Vec<_>>(), ["Friends", "Audit"]);
//!     let _client: FriendsHandle = services.client();
//! }
//! ```
//!
//! Two engine-event methods.
//!
//! ```compile_fail
//! use gsb_core::events::EngineEvent;
//! #[gsb_core::service]
//! pub trait Friends {
//!     #[gsb(engine_events)]
//!     async fn a(&self, e: EngineEvent);
//!     #[gsb(engine_events)]
//!     async fn b(&self, e: EngineEvent);
//! }
//! fn main() {}
//! ```
//!
//! An engine-event method with a call context.
//!
//! ```compile_fail
//! use gsb_core::calls::CallCx;
//! use gsb_core::events::EngineEvent;
//! #[gsb_core::service]
//! pub trait Friends {
//!     #[gsb(engine_events)]
//!     async fn on_engine(&self, cx: &CallCx, e: EngineEvent);
//! }
//! fn main() {}
//! ```
//!
//! An engine-event method that returns something (there is no answer).
//!
//! ```compile_fail
//! use gsb_core::events::EngineEvent;
//! #[gsb_core::service]
//! pub trait Friends {
//!     #[gsb(engine_events)]
//!     async fn on_engine(&self, e: EngineEvent) -> Result<(), String>;
//! }
//! fn main() {}
//! ```
//!
//! An engine-event method over another type.
//!
//! ```compile_fail
//! #[gsb_core::service]
//! pub trait Friends {
//!     #[gsb(engine_events)]
//!     async fn on_engine(&self, e: u32);
//! }
//! fn main() {}
//! ```
//!
//! An engine-event method taking the event by reference.
//!
//! ```compile_fail
//! use gsb_core::events::EngineEvent;
//! #[gsb_core::service]
//! pub trait Friends {
//!     #[gsb(engine_events)]
//!     async fn on_engine(&self, e: &EngineEvent);
//! }
//! fn main() {}
//! ```
//!
//! An engine-event method that is not `async`.
//!
//! ```compile_fail
//! use gsb_core::events::EngineEvent;
//! #[gsb_core::service]
//! pub trait Friends {
//!     #[gsb(engine_events)]
//!     fn on_engine(&self, e: EngineEvent);
//! }
//! fn main() {}
//! ```
//!
//! An unknown `#[gsb(..)]` key.
//!
//! ```compile_fail
//! use gsb_core::calls::CallCx;
//! #[gsb_core::service]
//! pub trait Friends {
//!     #[gsb(presence)]
//!     async fn count(&self, cx: &CallCx) -> Result<u32, String>;
//! }
//! fn main() {}
//! ```
//!
//! An impl whose engine method takes another type than the trait's.
//!
//! ```compile_fail
//! use gsb_core::events::{EngineEvent, EngineNotice};
//! #[gsb_core::service]
//! pub trait Friends {
//!     #[gsb(engine_events)]
//!     async fn on_engine(&self, e: EngineEvent);
//! }
//! struct Graph;
//! impl Friends for Graph {
//!     async fn on_engine(&self, _n: EngineNotice) {}
//! }
//! fn main() {}
//! ```
