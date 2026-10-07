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
//!     services.service(Bank::serve(Vault), |s| s.timeout(std::time::Duration::from_secs(1)));
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
//!     services.service(Bank::serve(NotABank), |s| s);
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
//!     services.service(Bank::serve(Vault), |s| s);
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
//!     services.service(Bank::serve(Vault), |s| s);
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
